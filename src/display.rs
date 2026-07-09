//! LCD 显示模块：1.3 寸 240x240 ST7789 SPI 屏幕
//!
//! 提供两个能力：
//! 1. [`init_display`]：完成 SPI + DC/RST/BL 的初始化，返回 `Display` 句柄
//! 2. [`ViewModel`] + [`render`]：把 `GamepadState` + 元数据画到屏上
//!
//! 布局（240x240）：
//! ```text
//!   ┌──────────────────────────┐
//!   │ RECV  seq=12345  gap=2   │  顶部状态行
//!   ├──────────────────────────┤
//!   │ Btn [1][2][3][4]         │  按钮方块
//!   │                          │
//!   │ Joy ( +12000, -50 )      │  摇杆 + 十字准星
//!   │                          │
//!   │ Knob1 32768  Knob2 16384 │  两个旋钮进度条
//!   └──────────────────────────┘
//! ```

use controller_protocol::{ButtonBits, GamepadState};
use embedded_graphics::{
  mono_font::{
    MonoTextStyle,
    ascii::{FONT_6X10, FONT_8X13_BOLD, FONT_10X20},
  },
  pixelcolor::Rgb565,
  prelude::*,
  primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle},
  text::{Alignment, Baseline, Text, TextStyleBuilder},
};

use crate::self_test::{ALL_ITEMS, SelfTestReport, SelfTestStatus};

/// 屏幕分辨率
pub const SCREEN_W: u16 = 240;
pub const SCREEN_H: u16 = 240;

// —— 主题配色 ——
const BG: Rgb565 = Rgb565::BLACK;
const FG: Rgb565 = Rgb565::WHITE;
const ACCENT: Rgb565 = Rgb565::CSS_CYAN;
const WARN: Rgb565 = Rgb565::CSS_ORANGE;
const OK: Rgb565 = Rgb565::CSS_LIME;
const OFF: Rgb565 = Rgb565::CSS_DIM_GRAY;

/// 渲染时需要的一切上下文
#[derive(Debug, Clone, Copy)]
pub struct ViewModel {
  /// 是否已收到过任何帧
  pub have_data: bool,
  /// 最近一帧的 seq
  pub last_seq: u32,
  /// 累计 seq gap 数
  pub gap_count: u32,
  /// 累计成功解码帧数
  pub ok_count: u32,
  /// 最近一帧的手柄状态
  pub state: GamepadState,
}

impl ViewModel {
  pub const fn empty() -> Self {
    Self {
      have_data: false,
      last_seq: 0,
      gap_count: 0,
      ok_count: 0,
      state: GamepadState::EMPTY,
    }
  }
}

// ============================================================
// 绘制辅助
// ============================================================

fn fill_screen<D>(display: &mut D, color: Rgb565) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  Rectangle::new(Point::zero(), Size::new(SCREEN_W as u32, SCREEN_H as u32))
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display)
}

fn draw_text<D>(
  display: &mut D,
  s: &str,
  pos: Point,
  color: Rgb565,
  bold: bool,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let text_style = TextStyleBuilder::new().baseline(Baseline::Top).build();
  if bold {
    let style = MonoTextStyle::new(&FONT_8X13_BOLD, color);
    Text::with_text_style(s, pos, style, text_style).draw(display)?;
  } else {
    let style = MonoTextStyle::new(&FONT_6X10, color);
    Text::with_text_style(s, pos, style, text_style).draw(display)?;
  }
  Ok(())
}

fn draw_big_number<D>(display: &mut D, s: &str, pos: Point, color: Rgb565) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let style = MonoTextStyle::new(&FONT_10X20, color);
  let text_style = TextStyleBuilder::new()
    .alignment(Alignment::Left)
    .baseline(Baseline::Top)
    .build();
  Text::with_text_style(s, pos, style, text_style).draw(display)?;
  Ok(())
}

/// 画一个按钮方块：8x13 字号 + 边框
fn draw_button<D>(display: &mut D, label: &str, pos: Point, pressed: bool) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let size = Size::new(44, 32);
  let (bg, fg, border) = if pressed {
    (OK, BG, ACCENT)
  } else {
    (BG, OFF, OFF)
  };

  let block_style = PrimitiveStyleBuilder::new()
    .fill_color(bg)
    .stroke_color(border)
    .stroke_width(2)
    .build();
  Rectangle::new(pos, size)
    .into_styled(block_style)
    .draw(display)?;

  // 居中文字（8x13 字号，label 长度 1-4 char）
  let text_x = pos.x + (size.width as i32 - (label.len() as i32) * 8) / 2;
  let text_y = pos.y + (size.height as i32 - 13) / 2;
  draw_text(display, label, Point::new(text_x, text_y), fg, true)?;
  Ok(())
}

/// 画一个水平进度条，val 归一化到 0..=u16::MAX
fn draw_bar<D>(
  display: &mut D,
  pos: Point,
  width: u32,
  height: u32,
  val: u16,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  // 外框
  Rectangle::new(pos, Size::new(width, height))
    .into_styled(
      PrimitiveStyleBuilder::new()
        .stroke_color(OFF)
        .stroke_width(1)
        .fill_color(BG)
        .build(),
    )
    .draw(display)?;

  // 填充
  let filled_w = (u32::from(val) * (width - 2)) / u32::from(u16::MAX);
  if filled_w > 0 {
    Rectangle::new(
      Point::new(pos.x + 1, pos.y + 1),
      Size::new(filled_w, height - 2),
    )
    .into_styled(PrimitiveStyle::with_fill(ACCENT))
    .draw(display)?;
  }
  Ok(())
}

/// 摇杆十字准星，joy_x/joy_y 是 i16，映射到 96x96 的区域
fn draw_joystick<D>(display: &mut D, pos: Point, joy_x: i16, joy_y: i16) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  const REGION: i32 = 80;
  let region_size = Size::new(REGION as u32, REGION as u32);

  // 背景框
  Rectangle::new(pos, region_size)
    .into_styled(
      PrimitiveStyleBuilder::new()
        .stroke_color(OFF)
        .stroke_width(1)
        .fill_color(BG)
        .build(),
    )
    .draw(display)?;

  // 十字线
  let cx = pos.x + REGION / 2;
  let cy = pos.y + REGION / 2;
  Rectangle::new(Point::new(pos.x, cy), Size::new(REGION as u32, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;
  Rectangle::new(Point::new(cx, pos.y), Size::new(1, REGION as u32))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;

  // 点：joy_x / joy_y 是 i16（-32768..=32767），映射到 -REGION/2..=REGION/2
  let px = cx + ((joy_x as i32) * (REGION / 2 - 4)) / i32::from(i16::MAX);
  let py = cy - ((joy_y as i32) * (REGION / 2 - 4)) / i32::from(i16::MAX);
  Rectangle::new(Point::new(px - 3, py - 3), Size::new(6, 6))
    .into_styled(PrimitiveStyle::with_fill(ACCENT))
    .draw(display)?;
  Ok(())
}

// ============================================================
// 主渲染入口
// ============================================================

/// 把整个 [`ViewModel`] 一次性画到屏幕上（简单全画法，避免脏矩形跟踪）。
pub fn render<D>(display: &mut D, vm: &ViewModel) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  fill_screen(display, BG)?;

  // —— 顶部状态行 ——
  let (title, title_color) = if vm.have_data {
    ("RECV", OK)
  } else {
    ("WAIT", WARN)
  };
  draw_text(display, title, Point::new(4, 4), title_color, true)?;

  let mut buf = heapless_str::<24>();
  let _ = core::fmt::write(&mut buf, format_args!("seq={}", vm.last_seq));
  draw_text(display, buf.as_str(), Point::new(50, 6), FG, false)?;

  let mut buf2 = heapless_str::<24>();
  let _ = core::fmt::write(&mut buf2, format_args!("gap={}", vm.gap_count));
  draw_text(
    display,
    buf2.as_str(),
    Point::new(140, 6),
    if vm.gap_count > 0 { WARN } else { OFF },
    false,
  )?;

  let mut buf3 = heapless_str::<24>();
  let _ = core::fmt::write(&mut buf3, format_args!("ok={}", vm.ok_count));
  draw_text(display, buf3.as_str(), Point::new(190, 6), OFF, false)?;

  // —— 按钮行 ——
  let btn_defs: [(ButtonBits, &str); 4] = [
    (ButtonBits::Btn1, "B1"),
    (ButtonBits::Btn2, "B2"),
    (ButtonBits::Btn3, "B3"),
    (ButtonBits::Btn4, "B4"),
  ];
  for (i, (bit, label)) in btn_defs.iter().enumerate() {
    let x = 8 + (i as i32) * 56;
    draw_button(display, label, Point::new(x, 28), vm.state.is_pressed(*bit))?;
  }

  // —— 摇杆区域 ——
  draw_joystick(display, Point::new(8, 72), vm.state.joy_x, vm.state.joy_y)?;

  // 摇杆数值
  let mut jx = heapless_str::<24>();
  let _ = core::fmt::write(&mut jx, format_args!("X:{:+6}", vm.state.joy_x));
  draw_text(display, jx.as_str(), Point::new(96, 80), FG, false)?;

  let mut jy = heapless_str::<24>();
  let _ = core::fmt::write(&mut jy, format_args!("Y:{:+6}", vm.state.joy_y));
  draw_text(display, jy.as_str(), Point::new(96, 96), FG, false)?;

  // —— 旋钮 ——
  draw_text(display, "K1", Point::new(8, 164), FG, true)?;
  draw_bar(display, Point::new(32, 164), 180, 12, vm.state.knob_1)?;
  let mut k1 = heapless_str::<24>();
  let _ = core::fmt::write(&mut k1, format_args!("{}", vm.state.knob_1));
  draw_text(display, k1.as_str(), Point::new(32, 180), OFF, false)?;

  draw_text(display, "K2", Point::new(8, 200), FG, true)?;
  draw_bar(display, Point::new(32, 200), 180, 12, vm.state.knob_2)?;
  let mut k2 = heapless_str::<24>();
  let _ = core::fmt::write(&mut k2, format_args!("{}", vm.state.knob_2));
  draw_text(display, k2.as_str(), Point::new(32, 216), OFF, false)?;

  // 用一下 big-number 引用，避免 dead_code
  let _ = draw_big_number::<D>;

  Ok(())
}

// ============================================================
// 自检页渲染
// ============================================================

/// 绘制自检进度页
///
/// 布局（240x240）：
/// ```text
///   ┌──────────────────────────┐
///   │       SELF-TEST          │  标题
///   ├──────────────────────────┤
///   │ HEAP    [ OK ]           │
///   │ LCD     [ OK ]           │
///   │ WIFI    [ .. ]           │
///   │ ESPNOW  [ .. ]           │
///   │ CODEC   [FAIL] alloc<512 │  失败附带原因
///   │ WATCH   [ .. ]           │
///   ├──────────────────────────┤
///   │ Result: OK / FAILED      │  汇总
///   └──────────────────────────┘
/// ```
pub fn render_self_test<D>(display: &mut D, report: &SelfTestReport) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  fill_screen(display, BG)?;

  // —— 标题 ——
  draw_text(display, "SELF-TEST", Point::new(72, 8), ACCENT, true)?;

  // 分隔线
  Rectangle::new(Point::new(4, 26), Size::new(232, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;

  // —— 逐项 ——
  let start_y: i32 = 40;
  let row_h: i32 = 22;
  for (i, item) in ALL_ITEMS.iter().enumerate() {
    let y = start_y + (i as i32) * row_h;

    // 项目名
    draw_text(display, item.label(), Point::new(8, y), FG, true)?;

    // 状态标签
    let status = report.status_of(*item);
    let (tag, color) = match status {
      SelfTestStatus::Pending => ("[ .. ]", OFF),
      SelfTestStatus::Ok => ("[ OK ]", OK),
      SelfTestStatus::Fail(_) => ("[FAIL]", WARN),
    };
    draw_text(display, tag, Point::new(96, y), color, true)?;

    // 失败原因
    if let SelfTestStatus::Fail(reason) = status {
      draw_text(display, reason, Point::new(160, y + 2), WARN, false)?;
    }
  }

  // 底部汇总
  let summary_y = start_y + (ALL_ITEMS.len() as i32) * row_h + 8;
  Rectangle::new(Point::new(4, summary_y), Size::new(232, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;

  let (summary, summary_color) = if report.any_fail() {
    ("FAILED - CHECK HW", WARN)
  } else if report.all_ok() {
    ("ALL OK", OK)
  } else {
    ("TESTING...", ACCENT)
  };
  draw_text(display, summary, Point::new(8, summary_y + 6), summary_color, true)?;

  Ok(())
}

// ============================================================

// ============================================================
// 无 alloc 的小字符串工具（基于固定数组的 core::fmt::Write）
// ============================================================

/// 便携的定长字符串缓冲，只在渲染时的临时格式化中使用
struct FixedStr<const N: usize> {
  buf: [u8; N],
  len: usize,
}

impl<const N: usize> FixedStr<N> {
  const fn new() -> Self {
    Self {
      buf: [0; N],
      len: 0,
    }
  }
  fn as_str(&self) -> &str {
    // SAFETY: 我们只追加了 UTF-8 字节
    unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
  }
}

impl<const N: usize> core::fmt::Write for FixedStr<N> {
  fn write_str(&mut self, s: &str) -> core::fmt::Result {
    let bytes = s.as_bytes();
    if self.len + bytes.len() > N {
      return Err(core::fmt::Error);
    }
    self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
    self.len += bytes.len();
    Ok(())
  }
}

/// 便捷构造
fn heapless_str<const N: usize>() -> FixedStr<N> {
  FixedStr::<N>::new()
}
