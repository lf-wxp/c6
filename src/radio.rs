//! ESP-NOW 接收模块
//!
//! 从 `EspNowReceiver` 循环读取原始 21B 报文，用 `controller-protocol::decode_frame`
//! 解码为 `Frame`，然后维护一个累计 `ViewModel`，并通过 `embassy_sync::watch::Watch`
//! 广播给显示 task。

use controller_protocol::{FRAME_LEN, decode_frame};
use defmt::{debug, info, warn};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, watch::Watch};
use esp_radio::esp_now::EspNowReceiver;

use crate::display::ViewModel;

/// 允许订阅的最大 consumer 数（这里只有一个渲染 task，1 足矣）
pub const WATCH_CONSUMERS: usize = 1;

/// 全局共享的 ViewModel 通道：接收 task 写入，显示 task 订阅
pub type StateWatch = Watch<CriticalSectionRawMutex, ViewModel, WATCH_CONSUMERS>;

/// ESP-NOW 接收主循环
///
/// 会永久运行，从收到的字节流中过滤出 21B 的 Frame 并解码；成功则更新 `vm` 并发布。
#[embassy_executor::task]
pub async fn recv_task(mut receiver: EspNowReceiver<'static>, watch: &'static StateWatch) -> ! {
  let sender = watch.sender();
  let mut vm = ViewModel::empty();
  // 首次上电就推一次「等待中」到通道，让屏能立刻画出初始画面
  sender.send(vm);

  info!("esp-now recv task started");

  loop {
    let data = receiver.receive_async().await;
    let bytes = data.data();

    if bytes.len() != FRAME_LEN {
      debug!("skip non-frame packet, len={}", bytes.len());
      continue;
    }

    // decode_frame 期望 &[u8; FRAME_LEN]
    let arr: &[u8; FRAME_LEN] = match bytes.try_into() {
      Ok(a) => a,
      Err(_) => continue,
    };

    match decode_frame(arr) {
      Ok(frame) => {
        // seq gap 检测
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
        sender.send(vm);
      }
      Err(err) => {
        warn!("decode_frame error: {:?}", defmt::Debug2Format(&err));
      }
    }
  }
}
