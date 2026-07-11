//! Peer 身份 / 控制面上下文
//!
//! 存放 receiver 侧的两块运行时状态：
//! 1. **`receiver_id`**：由手柄（controller）通过 `AssignId` 命令动态下发的逻辑 ID（0..=31），
//!    默认 [`INITIAL_RECEIVER_ID`]（= 0）；用于 [`controller_protocol::Frame::is_addressed_to`]
//!    过滤 `dest_mask`。
//! 2. **`replay`**：`AntiReplayWindow`——手柄 → receiver 方向 Command 的抗重放 64 位滑动窗，
//!    每收到一条 Command 就 `check_and_update(cmd.seq)`。
//!
//! ## 关于 `receiver_id` 持久化（NVS）的设计决策
//!
//! **当前实现是纯内存态，重启后 `receiver_id` 回到占位值 [`INITIAL_RECEIVER_ID`]（= 0）。
//! 这是有意为之，不是缺陷。**
//!
//! 依据 `docs/esp_now_receiver.md` 的设计意图：手柄（controller）侧会**持久化**
//! `receiver_id ↔ mac` 的映射，并在 discovery / Selecting 阶段周期性、按 MAC 重新下发
//! `AssignId`。因此：
//!
//! - 手柄用**广播**（`dest_mask = 0xFFFF_FFFF`）发状态时，本机重启后**立即能收**，零等待；
//! - 手柄用**定向**（`dest_mask` 只置位 `1 << N`，N ≠ 0）发状态时，重启后只需等一次
//!   `AssignId` 到位（通常几百 ms），期间少量定向帧被 `dest_mask` 过滤丢弃，无功能影响。
//!
//! 不引入 NVS 的代价边界（均为可接受的低严重度）：
//!
//! | 场景 | 后果 | 严重度 |
//! | --- | --- | --- |
//! | 重启后短暂收到定向帧 | 几帧被 `dest_mask` 过滤，手柄周期性重发自愈 | 低 |
//! | 手柄侧也不持久化 id 映射 | 每次重启后 id 可能变化，本机重分配一次 | 低 |
//! | 手柄发广播帧 | 无影响 | 无 |
//!
//! 与"不要过早优化"的原则一致：NVS 会引入 Flash 磨损均衡、写失败
//! 处理、断电一致性等复杂度，在"重启丢 id、几百 ms 自愈"的代价下属于过度工程。
//!
//! **仅当**出现以下需求之一时，才值得做 NVS 持久化（在 [`PeerCtx::assign`] 内把 `receiver_id`
//! 写入 NVS，并在 [`PeerCtx::new`] 时读回）：
//!
//! 1. 手柄明确不持久化 id↔mac 映射，且要求 receiver 重启后 id 必须稳定不变；
//! 2. 业务要求 receiver 在完全无手柄在场时，上电即用定向 id 收历史缓存帧；
//! 3. 已有 battery / 计数等需要跨重启保留的状态，顺带一起存。
//!
//! 当前项目均不满足上述条件。

use controller_protocol::AntiReplayWindow;

/// ESP-NOW 广播地址（`FF:FF:FF:FF:FF:FF`）：AnnounceReply 与其它响应帧都走广播发出。
pub const BROADCAST: [u8; 6] = [0xFF; 6];

/// 首次上电前的占位 receiver_id。
///
/// 参考 `docs/esp_now_receiver.md`：
/// > "首次上电前可以用一个占位值（例如 0），收到 AssignId 后再持久化到 NVS。"
///
/// 值为 0 时 `1 << 0 = 0x0000_0001`，广播帧 (`dest_mask = 0xFFFF_FFFF`) 依旧命中。
pub const INITIAL_RECEIVER_ID: u8 = 0;

/// receiver_id 上限（0..=31，对应 `dest_mask: u32` 的 32 个位）。
pub const RECEIVER_ID_MAX: u8 = 31;

/// AnnounceReply 里的 `role_tag`：3 字节 ASCII，标识本 receiver 的角色。
///
/// 本项目是 "LCD Display Sink"，用 `lcd` 表示；不足右侧补 0。
pub const ROLE_TAG: [u8; 3] = *b"lcd";

/// 控制面运行时上下文：receiver 侧收到手柄的 Command 时用到的状态。
///
/// - 非 `Copy`：内部含 `AntiReplayWindow`（64 位位图），显式借用避免误复制导致重放窗漂移。
/// - 非 `Send` / `Sync` 约束由使用侧保证（当前只在单一 `recv_task` 里可变借用）。
pub struct PeerCtx {
  /// 本机当前的逻辑 receiver_id。收到 `AssignId { mac == own_mac, .. }` 后被覆写。
  receiver_id: u8,
  /// controller→receiver 方向 Command 的抗重放窗。
  replay: AntiReplayWindow,
  /// 是否收到过 `AssignId` 分配。用于在 UI 上区分"初始占位 id"和"已被手柄分配"。
  assigned: bool,
}

impl PeerCtx {
  /// 用 [`INITIAL_RECEIVER_ID`] + 空重放窗构造一个新的 `PeerCtx`。
  #[must_use]
  pub const fn new() -> Self {
    Self {
      receiver_id: INITIAL_RECEIVER_ID,
      replay: AntiReplayWindow::new(),
      assigned: false,
    }
  }

  /// 当前 `receiver_id`（用于 `frame.is_addressed_to(id)` 过滤）。
  #[must_use]
  pub const fn receiver_id(&self) -> u8 {
    self.receiver_id
  }

  /// 手柄是否已经通过 `AssignId` 给本机分配过 ID。
  #[must_use]
  pub const fn is_assigned(&self) -> bool {
    self.assigned
  }

  /// 处理 `AssignId { mac, receiver_id }`：仅当 mac 与自身一致才吃下。
  ///
  /// 返回 `true` 表示"匹配 + 已更新"，`false` 表示"MAC 不是给我的"。
  ///
  /// 越界 (`receiver_id > 31`) 直接丢弃，避免 `dest_mask` 位图越位。
  pub fn assign(&mut self, own_mac: [u8; 6], target_mac: [u8; 6], receiver_id: u8) -> bool {
    if target_mac != own_mac {
      return false;
    }
    if receiver_id > RECEIVER_ID_MAX {
      return false;
    }
    self.receiver_id = receiver_id;
    self.assigned = true;
    true
  }

  /// 对 controller→receiver 方向的 Command 做抗重放检查。
  ///
  /// 通过则返回 `Ok(())` 并推进窗口；已见/过老则返回 `Err`，调用方应静默丢弃该命令。
  pub fn check_replay(&mut self, seq: u32) -> Result<(), controller_protocol::ReplayError> {
    self.replay.check_and_update(seq)
  }
}

impl defmt::Format for PeerCtx {
  fn format(&self, fmt: defmt::Formatter) {
    defmt::write!(
      fmt,
      "PeerCtx {{ receiver_id={}, assigned={} }}",
      self.receiver_id,
      self.assigned
    );
  }
}

impl Default for PeerCtx {
  fn default() -> Self {
    Self::new()
  }
}
