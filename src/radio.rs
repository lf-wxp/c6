//! ESP-NOW 接收模块（`controller-protocol` v0.2.0）
//!
//! 本模块承担 receiver 侧的全部空口业务：
//!
//! 1. **Frame（`0xC71E`, 25 B）** —— 手柄广播状态帧 → 解码 → `dest_mask` 过滤 → 更新 [`ViewModel`]。
//! 2. **Command（`0xCB01`, 24 B）** —— 手柄下发的控制命令；本 receiver 只关心两种：
//!    - `Announce`：广播 `AnnounceReply { mac, rssi_dbm=-127, role_tag=b"lcd" }` 让手柄发现本机。
//!    - `AssignId { mac, receiver_id }`：`mac == own_mac` 时把 [`PeerCtx`] 里的 id 更新。
//!
//!    其它 Command（Nop / LedBlink / ShowToast / ...）本 receiver 视为「发给手柄本体的
//!    命令」，静默忽略——符合官方 `docs/esp_now_receiver.md` 的「进阶：响应 Announce &
//!    接受 AssignId」章节。
//! 3. **其它 magic**（如 Response 广播的 NonceHello / 空气里的干扰帧）—— 静默忽略。
//!
//! ## 密钥依赖
//!
//! Command / Response 都走 HMAC-SHA256 校验（截断 4B tag），密钥由 `controller-protocol`
//! 的 `build.rs` 在编译期从环境变量 `CONTROLLER_SECRET_V1/V2` 注入。
//!
//! 如果 `.cargo/config.toml` 里密钥与手柄侧不一致：`decode_command` 会返回 `AuthFailed`，
//! 本模块**会 debug 记录一次，然后继续跑**——Frame 通路完全不受影响（Frame 不带 HMAC）。
//!
//! 想临时禁用控制面：把主循环 `match` 中 `COMMAND_LEN` 分支整段跳过即可——Frame 通路
//! 由独立的 [`handle_frame`] 处理，完全不受影响。

use controller_protocol::{
  COMMAND_LEN, CommandBody, FRAME_LEN, RESPONSE_LEN, ReplayError, decode_command, decode_frame,
  encode_response,
};
use defmt::{debug, info, warn};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use esp_radio::esp_now::{EspNowReceiver, EspNowSender};

use crate::display::ViewModel;
use crate::peer::{BROADCAST, PeerCtx, RECEIVER_ID_MAX, ROLE_TAG};

/// 允许订阅的最大 consumer 数（这里只有一个渲染 task，1 足矣）
pub const WATCH_CONSUMERS: usize = 1;

/// 全局共享的 ViewModel 通道：接收 task 写入，显示 task 订阅
pub type StateWatch = Watch<CriticalSectionRawMutex, ViewModel, WATCH_CONSUMERS>;

/// Frame magic（`0xC71E`，LE）
const FRAME_MAGIC_LE: [u8; 2] = [0x1E, 0xC7];
/// Command magic（`0xCB01`，LE）
const COMMAND_MAGIC_LE: [u8; 2] = [0x01, 0xCB];

/// ESP-NOW 接收主循环
///
/// 会永久运行；用 magic + 长度做双重过滤，路由到 Frame / Command 两条业务线。
/// 每次业务处理后把最新 [`ViewModel`] 通过 `watch` 广播给显示 task。
///
/// # 参数
///
/// - `receiver` / `sender`：`esp_now.split()` 拆出来的读写半边。`sender` 只用于
///   在收到 `Announce` 时广播 `AnnounceReply`；其它情况完全不占用。
/// - `own_mac`：本机 MAC-48（从 `interfaces.station.mac_address()` 读取），
///   `AnnounceReply.mac` 与 `AssignId.mac == own_mac` 两处都要用。
/// - `watch`：`ViewModel` 通道。
#[embassy_executor::task]
pub async fn recv_task(
  mut receiver: EspNowReceiver<'static>,
  mut sender: EspNowSender<'static>,
  own_mac: [u8; 6],
  watch: &'static StateWatch,
) -> ! {
  let tx = watch.sender();
  let mut vm = ViewModel::empty();
  let mut peer = PeerCtx::new();
  // 首次上电就推一次「等待中」到通道，让屏能立刻画出初始画面
  tx.send(vm);

  info!(
    "esp-now recv task started, own_mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
    own_mac[0], own_mac[1], own_mac[2], own_mac[3], own_mac[4], own_mac[5]
  );

  loop {
    let pkt = receiver.receive_async().await;
    let bytes = pkt.data();

    // ---- 用 magic + 长度双重快速分派 ----
    if bytes.len() < 2 {
      continue;
    }
    let magic = [bytes[0], bytes[1]];

    match (magic, bytes.len()) {
      (m, FRAME_LEN) if m == FRAME_MAGIC_LE => {
        handle_frame(bytes, &peer, &mut vm, &tx);
      }
      (m, COMMAND_LEN) if m == COMMAND_MAGIC_LE => {
        handle_command(bytes, &mut peer, &mut vm, &mut sender, own_mac, &tx).await;
      }
      _ => {
        debug!(
          "skip packet: magic=[{:02x},{:02x}] len={}",
          bytes[0],
          bytes[1],
          bytes.len()
        );
      }
    }
  }
}

/// 处理 25 B Frame：解码 + `dest_mask` 过滤 + seq gap 检测 + `ViewModel` 更新。
fn handle_frame(
  bytes: &[u8],
  peer: &PeerCtx,
  vm: &mut ViewModel,
  tx: &embassy_sync::watch::Sender<'static, CriticalSectionRawMutex, ViewModel, WATCH_CONSUMERS>,
) {
  // decode_frame 期望 &[u8; FRAME_LEN]
  let Ok(arr) = <&[u8; FRAME_LEN]>::try_from(bytes) else {
    return;
  };

  let frame = match decode_frame(arr) {
    Ok(f) => f,
    Err(err) => {
      warn!("decode_frame error: {:?}", defmt::Debug2Format(&err));
      return;
    }
  };

  // ---- dest_mask 过滤（官方 receiver.md 关键点 4）----
  //
  // `is_addressed_to` 是 const fn，编译器可以内联，零 CPU 开销。
  if !frame.is_addressed_to(peer.receiver_id()) {
    vm.filtered_count = vm.filtered_count.saturating_add(1);
    // 不发布 vm——过滤帧不代表业务状态变化；下一帧要么是我的（正常刷新）、
    // 要么继续被过滤（累计到下次真正的更新时一起被显示）
    return;
  }

  // ---- seq gap 检测 ----
  if vm.have_data {
    let expected = vm.last_seq.wrapping_add(1);
    if frame.header.seq != expected {
      let missing = frame.header.seq.wrapping_sub(expected);
      warn!(
        "seq gap: expected={}, got={}, missing={}",
        expected, frame.header.seq, missing
      );
      vm.gap_count = vm.gap_count.saturating_add(1);
    }
  }

  vm.have_data = true;
  vm.last_seq = frame.header.seq;
  vm.ok_count = vm.ok_count.saturating_add(1);
  vm.state = frame.payload;
  tx.send(*vm);
}

/// 处理 24 B Command：Announce / AssignId 两路，其它静默忽略。
async fn handle_command(
  bytes: &[u8],
  peer: &mut PeerCtx,
  vm: &mut ViewModel,
  sender: &mut EspNowSender<'static>,
  own_mac: [u8; 6],
  tx: &embassy_sync::watch::Sender<'static, CriticalSectionRawMutex, ViewModel, WATCH_CONSUMERS>,
) {
  let cmd = match decode_command(bytes) {
    Ok(c) => c,
    Err(err) => {
      // AuthFailed / BadCrc / UnsupportedVersion / ...：静默忽略但打一条 debug
      // （避免密钥不匹配时刷屏 warn）
      debug!("command decode error: {:?}", defmt::Debug2Format(&err));
      return;
    }
  };

  // 抗重放窗（per receiver：controller→我 方向）。
  // `Announce` 是手柄周期性广播的发现帧，seq 可能重复/回绕，对其豁免重放检查，
  // 否则本机只会回一次 AnnounceReply 后即在窗口内「失联」；其余命令照常防重放。
  if !matches!(cmd.kind, CommandBody::Announce)
    && let Err(rerr) = peer.check_replay(cmd.seq)
  {
    debug!(
      "replay rejected: seq={} err={:?}",
      cmd.seq,
      defmt::Debug2Format::<ReplayError>(&rerr)
    );
    return;
  }

  match cmd.kind {
    CommandBody::Announce => {
      // 广播 AnnounceReply。rssi_dbm 手上没有可靠来源（esp-radio 的 RxControlInfo
      // 需要额外 API 才能拿到，此处保留 -127 = "未知"，与协议注释一致）。
      let reply = controller_protocol::CommandResponse::announce_reply(
        cmd.seq, cmd.key_id, own_mac, -127, ROLE_TAG,
      );
      let wire: [u8; RESPONSE_LEN] = encode_response(&reply);
      match sender.send_async(&BROADCAST, &wire).await {
        Ok(()) => {
          vm.reply_count = vm.reply_count.saturating_add(1);
          info!("AnnounceReply sent (req_seq={})", cmd.seq);
          tx.send(*vm);
        }
        Err(err) => {
          warn!("AnnounceReply send failed: {:?}", defmt::Debug2Format(&err));
        }
      }
    }
    CommandBody::AssignId { mac, receiver_id } => {
      if peer.assign(own_mac, mac, receiver_id) {
        vm.receiver_id = peer.receiver_id();
        vm.assigned = peer.is_assigned();
        info!("AssignId accepted → receiver_id={}", receiver_id);
        tx.send(*vm);
      } else {
        debug!(
          "AssignId ignored (mac mismatch or id>{}): id={}",
          RECEIVER_ID_MAX, receiver_id
        );
      }
    }
    // 其它命令（Nop / LedBlink / SetSensitivity / ShowToast / SetBatteryMode）都是
    // "发给手柄本体的"控制命令；receiver 静默忽略，不产生 Response。
    other => {
      debug!(
        "command kind ignored by receiver: {:?}",
        defmt::Debug2Format(&other)
      );
    }
  }
}
