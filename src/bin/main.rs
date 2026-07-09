#![no_std]
#![no_main]
#![deny(
  clippy::mem_forget,
  reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use c6::display::{SCREEN_H, SCREEN_W, ViewModel, render};
use c6::post;
use c6::radio::{StateWatch, recv_task};
use c6::sdcard;
use c6::self_test::{
  SelfTestItem, SelfTestReport, SelfTestStatus, run_codec_check, run_heap_check,
};
use core::cell::RefCell;
use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::RefCellDevice;
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
/// 共享 SPI 总线（LCD + SD 卡复用）；`RefCell` 提供内部可变性，
/// 由 `embedded-hal-bus::spi::RefCellDevice` 在使用时借出。
///
/// 用 `StaticCell` 提升到 `'static`，让 `RefCellDevice<'static, ..>` 能贯穿整个 main 生命周期。
static SPI_BUS: StaticCell<RefCell<Spi<'static, esp_hal::Blocking>>> = StaticCell::new();

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
  // SPI2：LCD + SD 共用同一条总线
  //
  // 引脚：MOSI=GPIO6 / SCLK=GPIO7 / MISO=GPIO5（SD 用）；
  // 频率取 LCD 与 SD 共同上限的稳妥值 20 MHz。
  // -----------------------------------------------------------------
  let spi = Spi::new(
    peripherals.SPI2,
    SpiConfig::default()
      .with_frequency(Rate::from_mhz(20))
      .with_mode(SpiMode::_0),
  )
  .expect("spi cfg")
  .with_sck(peripherals.GPIO7)
  .with_mosi(peripherals.GPIO6)
  .with_miso(peripherals.GPIO5);

  // 把 SPI 提升到 'static，交给 RefCell 让 LCD / SD 分时复用
  let spi_bus: &'static RefCell<Spi<'static, esp_hal::Blocking>> = SPI_BUS.init(RefCell::new(spi));

  // 各设备的 CS / DC / RES / BL
  let lcd_cs = Output::new(peripherals.GPIO14, Level::High, OutputConfig::default());
  let lcd_dc = Output::new(peripherals.GPIO15, Level::Low, OutputConfig::default());
  let lcd_rst = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());
  let mut backlight = Output::new(peripherals.GPIO22, Level::Low, OutputConfig::default());
  let sd_cs = Output::new(peripherals.GPIO4, Level::High, OutputConfig::default());

  // -----------------------------------------------------------------
  // LCD：从共享总线借一路 SpiDevice + 独占 CS
  // -----------------------------------------------------------------
  let lcd_spi = RefCellDevice::new(spi_bus, lcd_cs, Delay::new()).expect("lcd spi device");
  let buffer: &'static mut [u8; 1024] = DISPLAY_BUF.init([0_u8; 1024]);

  let di = SpiInterface::new(lcd_spi, lcd_dc, buffer);
  let mut delay = Delay::new();
  let mut display = Builder::new(ST7789, di)
    .display_size(SCREEN_W, SCREEN_H)
    .orientation(Orientation::new().rotate(Rotation::Deg0))
    .invert_colors(ColorInversion::Inverted)
    .reset_pin(lcd_rst)
    .init(&mut delay)
    .expect("lcd init");

  // 打开背光
  backlight.set_high();
  info!("LCD initialized (240x240 ST7789)");

  // -----------------------------------------------------------------
  // 自检 (POST)
  // -----------------------------------------------------------------
  let mut report = SelfTestReport::new();

  // 初始画面（全部 pending）
  post::refresh(&mut display, &report).await;

  // 阶段 1：Heap / LCD / Codec
  post::step(
    &mut display,
    &mut report,
    SelfTestItem::Heap,
    run_heap_check(),
  )
  .await;
  post::step(
    &mut display,
    &mut report,
    SelfTestItem::Lcd,
    SelfTestStatus::Ok,
  )
  .await;
  post::step(
    &mut display,
    &mut report,
    SelfTestItem::Codec,
    run_codec_check(),
  )
  .await;

  // SD 卡（可选外设：无卡不阻塞主流程）
  let sd_spi = RefCellDevice::new(spi_bus, sd_cs, Delay::new()).expect("sd spi device");
  let (sd_status, _sd_info) = sdcard::try_mount(sd_spi, Delay::new());
  post::step(&mut display, &mut report, SelfTestItem::Sd, sd_status).await;

  // WiFi
  let (mut _wifi_controller, interfaces) =
    match esp_radio::wifi::new(peripherals.WIFI, Default::default()) {
      Ok(pair) => {
        post::step(
          &mut display,
          &mut report,
          SelfTestItem::Wifi,
          SelfTestStatus::Ok,
        )
        .await;
        pair
      }
      Err(e) => {
        warn!("wifi init failed: {:?}", defmt::Debug2Format(&e));
        post::step(
          &mut display,
          &mut report,
          SelfTestItem::Wifi,
          SelfTestStatus::Fail("init err"),
        )
        .await;
        // WiFi 是硬依赖，画完自检页后停在这里
        loop {
          Timer::after(Duration::from_secs(1)).await;
        }
      }
    };

  // ESP-NOW split（当前 API 不返回 Result，能拿到 receiver 即 OK）
  let (_manager, _sender, receiver) = interfaces.esp_now.split();
  post::step(
    &mut display,
    &mut report,
    SelfTestItem::EspNow,
    SelfTestStatus::Ok,
  )
  .await;

  // Watch 通道：探测一次 receiver 可用性
  let watch: &'static StateWatch = STATE_WATCH.init(StateWatch::new());
  let watch_status = if watch.receiver().is_some() {
    SelfTestStatus::Ok
  } else {
    SelfTestStatus::Fail("no slot")
  };
  post::step(&mut display, &mut report, SelfTestItem::Watch, watch_status).await;

  // 自检结果汇总
  if report.any_critical_fail() {
    warn!("self-test FAILED (critical), halting");
    loop {
      Timer::after(Duration::from_secs(1)).await;
    }
  }
  if report.any_fail() {
    info!("self-test finished with non-critical warnings");
  } else {
    info!("self-test ALL OK");
  }
  // 让用户看清 "ALL OK" 之后再进入正常界面
  Timer::after(Duration::from_millis(600)).await;

  spawner.spawn(recv_task(receiver, watch).expect("build recv_task"));

  // -----------------------------------------------------------------
  // 渲染主循环：订阅 Watch，收到就重画
  // -----------------------------------------------------------------
  let mut receiver_ch = watch.receiver().expect("watch receiver");
  let mut last_vm = ViewModel::empty();
  // 先画一次初始 (WAIT) 画面
  if render(&mut display, &last_vm).is_err() {
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
