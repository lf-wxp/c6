#![no_std]
#![no_main]
#![deny(
  clippy::mem_forget,
  reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use c6::display::{SCREEN_H, SCREEN_W, ViewModel, render, render_self_test};
use c6::radio::{StateWatch, recv_task};
use c6::self_test::{
  SelfTestItem, SelfTestReport, SelfTestStatus, run_codec_check, run_heap_check,
};
use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::{
  clock::CpuClock,
  delay::Delay,
  gpio::{Level, Output, OutputConfig},
  spi::{
    Mode as SpiMode,
    master::{Config as SpiConfig, Spi},
  },
  time::Rate,
  timer::timg::TimerGroup,
};
use mipidsi::{
  Builder,
  interface::SpiInterface,
  models::ST7789,
  options::{ColorInversion, Orientation, Rotation},
};
use panic_rtt_target as _;
use static_cell::StaticCell;

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// Watch 通道：'static 生命周期，需通过 StaticCell 惰性初始化
static STATE_WATCH: StaticCell<StateWatch> = StaticCell::new();
/// mipidsi SpiInterface 内部需要一个字节缓冲区，用于批量像素写入
static DISPLAY_BUF: StaticCell<[u8; 1024]> = StaticCell::new();

#[allow(
  clippy::large_stack_frames,
  reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  // -----------------------------------------------------------------
  // 堆 / RTOS
  // -----------------------------------------------------------------
  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
  // COEX + WiFi 需要额外堆
  esp_alloc::heap_allocator!(size: 64 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

  info!("Embassy initialized!");

  // -----------------------------------------------------------------
  // LCD (SPI2)：MOSI=GPIO6, CLK=GPIO7, CS=GPIO14, DC=GPIO15, RES=GPIO21, BL=GPIO22
  // -----------------------------------------------------------------
  let spi = Spi::new(
    peripherals.SPI2,
    SpiConfig::default()
      .with_frequency(Rate::from_mhz(40))
      .with_mode(SpiMode::_0),
  )
  .expect("spi cfg")
  .with_sck(peripherals.GPIO7)
  .with_mosi(peripherals.GPIO6);

  let cs = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());
  let dc = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());
  let rst = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());
  let mut backlight = Output::new(peripherals.GPIO22, Level::Low, OutputConfig::default());

  // ExclusiveDevice 把 SpiBus + CS 组合成 SpiDevice
  let spi_device = ExclusiveDevice::new(spi, cs, Delay::new()).expect("spi device");
  let buffer: &'static mut [u8; 1024] = DISPLAY_BUF.init([0_u8; 1024]);

  let di = SpiInterface::new(spi_device, dc, buffer);
  let mut delay = Delay::new();
  let mut display = Builder::new(ST7789, di)
    .display_size(SCREEN_W, SCREEN_H)
    .orientation(Orientation::new().rotate(Rotation::Deg0))
    .invert_colors(ColorInversion::Inverted)
    .reset_pin(rst)
    .init(&mut delay)
    .expect("lcd init");

  // 打开背光
  backlight.set_high();
  info!("LCD initialized (240x240 ST7789)");

  // -----------------------------------------------------------------
  // 自检 (POST) — 阶段 1：Heap / LCD / Codec
  // -----------------------------------------------------------------
  let mut report = SelfTestReport::new();

  // 初始画面（全部 pending）
  if render_self_test(&mut display, &report).is_err() {
    warn!("render_self_test error");
  }

  // Heap
  report.mark(SelfTestItem::Heap, run_heap_check());
  let _ = render_self_test(&mut display, &report);
  Timer::after(Duration::from_millis(120)).await;

  // LCD（走到这一步说明 LCD 已经能刷新 → OK）
  report.mark(SelfTestItem::Lcd, SelfTestStatus::Ok);
  let _ = render_self_test(&mut display, &report);
  Timer::after(Duration::from_millis(120)).await;

  // Codec loopback（依赖 controller-protocol 编解码，能间接检查密钥是否已注入）
  report.mark(SelfTestItem::Codec, run_codec_check());
  let _ = render_self_test(&mut display, &report);
  Timer::after(Duration::from_millis(120)).await;

  // -----------------------------------------------------------------
  // WiFi + ESP-NOW
  // -----------------------------------------------------------------
  let (mut _wifi_controller, interfaces) = match esp_radio::wifi::new(peripherals.WIFI, Default::default()) {
    Ok(pair) => {
      report.mark(SelfTestItem::Wifi, SelfTestStatus::Ok);
      let _ = render_self_test(&mut display, &report);
      pair
    }
    Err(e) => {
      warn!("wifi init failed: {:?}", defmt::Debug2Format(&e));
      report.mark(SelfTestItem::Wifi, SelfTestStatus::Fail("init err"));
      // WiFi 是硬依赖，画完自检页后停在这里
      let _ = render_self_test(&mut display, &report);
      loop {
        Timer::after(Duration::from_secs(1)).await;
      }
    }
  };
  Timer::after(Duration::from_millis(120)).await;

  // ESP-NOW split（当前 API 不返回 Result，能拿到 receiver 即 OK）
  let (_manager, _sender, receiver) = interfaces.esp_now.split();
  report.mark(SelfTestItem::EspNow, SelfTestStatus::Ok);
  let _ = render_self_test(&mut display, &report);
  Timer::after(Duration::from_millis(120)).await;

  // -----------------------------------------------------------------
  // 拉起接收 task
  // -----------------------------------------------------------------
  let watch: &'static StateWatch = STATE_WATCH.init(StateWatch::new());
  // 尝试拿一个 receiver 验证通道可用（探测后立即 drop，避免占用 WATCH_CONSUMERS 名额）
  match watch.receiver() {
    Some(_probe) => {
      report.mark(SelfTestItem::Watch, SelfTestStatus::Ok);
    }
    None => {
      report.mark(SelfTestItem::Watch, SelfTestStatus::Fail("no slot"));
    }
  }
  let _ = render_self_test(&mut display, &report);
  Timer::after(Duration::from_millis(120)).await;

  // 自检结果汇总
  if report.any_fail() {
    warn!("self-test FAILED, halting");
    loop {
      Timer::after(Duration::from_secs(1)).await;
    }
  }
  info!("self-test ALL OK");
  // 让用户看清 "ALL OK" 之后再进入正常界面
  Timer::after(Duration::from_millis(600)).await;

  spawner.spawn(recv_task(receiver, watch).expect("build recv_task"));

  // -----------------------------------------------------------------
  // 渲染主循环：订阅 Watch，收到就重画
  // -----------------------------------------------------------------
  let mut receiver_ch = watch.receiver().expect("watch receiver");
  let mut last_vm = ViewModel::empty();
  // 先画一次初始 (WAIT) 画面
  if let Err(_e) = render(&mut display, &last_vm) {
    warn!("render error at boot");
  }

  loop {
    // 等新数据；如超过 500ms 也重画一次（避免屏幕不刷）
    match embassy_futures::select::select(
      receiver_ch.changed(),
      Timer::after(Duration::from_millis(500)),
    )
    .await
    {
      embassy_futures::select::Either::First(vm) => {
        last_vm = vm;
      }
      embassy_futures::select::Either::Second(_) => {}
    }

    if render(&mut display, &last_vm).is_err() {
      warn!("render error");
    }
  }
}
