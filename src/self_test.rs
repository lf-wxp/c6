//! 上电自检 (Power-On Self Test, POST)
//!
//! 在设备正常工作前，逐项检测关键子系统是否可用：
//! - Heap : 是否能分配一小段堆内存
//! - Lcd  : LCD 是否成功初始化（由外部传入结果）
//! - Wifi : WiFi controller 是否成功创建（由外部传入结果）
//! - Now  : ESP-NOW 收发器是否成功创建（由外部传入结果）
//! - Codec: `controller-protocol` 编解码 loopback 是否 OK
//!   （能间接验证 CRC/HMAC/密钥已正确注入）
//! - Ch   : 全局 Watch 通道是否可正常发布/订阅
//!
//! 用法：
//! ```ignore
//! let mut report = SelfTestReport::new();
//! report.mark(SelfTestItem::Heap, run_heap_check());
//! report.mark(SelfTestItem::Codec, run_codec_check());
//! // ... 每个 mark 后可用 render_self_test 刷屏
//! ```

use controller_protocol::{Frame, GamepadState, decode_frame, encode_frame};

/// 自检的项目，`ITEM_COUNT` 需要与该枚举个数保持一致
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTestItem {
  Heap,
  Lcd,
  Wifi,
  EspNow,
  Codec,
  Watch,
}

/// 项目总数，用于固定大小数组
pub const ITEM_COUNT: usize = 6;

/// 全部项目按屏幕上从上到下的显示顺序
pub const ALL_ITEMS: [SelfTestItem; ITEM_COUNT] = [
  SelfTestItem::Heap,
  SelfTestItem::Lcd,
  SelfTestItem::Wifi,
  SelfTestItem::EspNow,
  SelfTestItem::Codec,
  SelfTestItem::Watch,
];

impl SelfTestItem {
  /// 用于屏幕显示的短名
  pub const fn label(self) -> &'static str {
    match self {
      Self::Heap => "HEAP  ",
      Self::Lcd => "LCD   ",
      Self::Wifi => "WIFI  ",
      Self::EspNow => "ESPNOW",
      Self::Codec => "CODEC ",
      Self::Watch => "WATCH ",
    }
  }
}

/// 单项检测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfTestStatus {
  Pending,
  Ok,
  /// 失败并给出简短原因（`'static &str`，避免堆）
  Fail(&'static str),
}

impl SelfTestStatus {
  pub const fn is_fail(self) -> bool {
    matches!(self, Self::Fail(_))
  }
}

/// 自检报告：`ALL_ITEMS` 的状态数组
#[derive(Debug, Clone, Copy)]
pub struct SelfTestReport {
  pub items: [SelfTestStatus; ITEM_COUNT],
}

impl SelfTestReport {
  pub const fn new() -> Self {
    Self {
      items: [SelfTestStatus::Pending; ITEM_COUNT],
    }
  }

  pub fn mark(&mut self, item: SelfTestItem, status: SelfTestStatus) {
    let idx = ALL_ITEMS.iter().position(|&i| i == item).unwrap_or(0);
    self.items[idx] = status;
  }

  pub fn status_of(&self, item: SelfTestItem) -> SelfTestStatus {
    let idx = ALL_ITEMS.iter().position(|&i| i == item).unwrap_or(0);
    self.items[idx]
  }

  /// 是否已经至少有一项失败
  pub fn any_fail(&self) -> bool {
    self.items.iter().any(|s| s.is_fail())
  }

  /// 是否全部通过（无 Pending 且无 Fail）
  pub fn all_ok(&self) -> bool {
    self.items.iter().all(|s| matches!(s, SelfTestStatus::Ok))
  }
}

impl Default for SelfTestReport {
  fn default() -> Self {
    Self::new()
  }
}

// ============================================================
// 具体检测项（那些**无外部依赖**、可直接在 self_test 里跑的）
// ============================================================

/// Heap: 尝试用 alloc 分配一小段内存
pub fn run_heap_check() -> SelfTestStatus {
  extern crate alloc;
  use alloc::vec::Vec;

  // 尝试 512B 分配 → 立刻 drop
  // esp-alloc 在 OOM 时会直接 panic，所以能运行到容量检查就说明堆可用。
  let v: Vec<u8> = Vec::with_capacity(512);
  if v.capacity() < 512 {
    return SelfTestStatus::Fail("alloc<512B");
  }
  drop(v);
  SelfTestStatus::Ok
}

/// Codec: 用 controller-protocol 做一次 encode -> decode round-trip
///
/// 若密钥未正确注入（例如全 0），`decode_frame` 会在 HMAC 校验时失败；
/// 因此这一项能间接验证：
///  - CRC 计算实现可用
///  - HMAC 密钥已被正确 embed
///  - 会话 nonce 已 init（这一点上层调用需先 init_session_nonce）
pub fn run_codec_check() -> SelfTestStatus {
  let original = Frame::new(0x1234_5678, GamepadState::EMPTY);
  let bytes = encode_frame(&original);
  match decode_frame(&bytes) {
    Ok(decoded) => {
      if decoded.header.seq == original.header.seq && decoded.payload == original.payload {
        SelfTestStatus::Ok
      } else {
        SelfTestStatus::Fail("decode!=orig")
      }
    }
    Err(_) => SelfTestStatus::Fail("decode err"),
  }
}
