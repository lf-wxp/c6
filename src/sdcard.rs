//! SD 卡（FAT16/FAT32）读取模块
//!
//! 通过 [`embedded-sdmmc`] 在 SPI2 上驱动 SD 卡，与 LCD 共用总线（GPIO6=MOSI /
//! GPIO7=SCLK），SD 独有引脚：
//! - `MISO` = GPIO5
//! - `CS`   = GPIO4
//!
//! ## 设计
//!
//! - 使用 `embedded_hal_bus::spi::RefCellDevice` 让 LCD 与 SD **共享** 一条 `SpiBus`
//! - 单线程/协程使用，SD 卡访问在自检阶段一次性完成
//! - 无实时时钟，`TimeSource` 用固定时间戳（FAT 记录都是 2024-01-01 00:00:00）

use defmt::{info, warn};
use embedded_hal::{delay::DelayNs, spi::SpiDevice};
use embedded_sdmmc::{TimeSource, Timestamp};

use crate::self_test::SelfTestStatus;

/// 简易 `TimeSource`：无实时时钟时的占位（固定时间戳）
///
/// FAT 文件系统里所有新建/修改时间都会记为该常量。生产环境如需真时钟，
/// 可换成读 RTC 的实现。
#[derive(Debug, Clone, Copy, Default)]
pub struct DummyTimeSource;

impl TimeSource for DummyTimeSource {
  fn get_timestamp(&self) -> Timestamp {
    Timestamp {
      year_since_1970: 54, // 1970 + 54 = 2024
      zero_indexed_month: 0,
      zero_indexed_day: 0,
      hours: 0,
      minutes: 0,
      seconds: 0,
    }
  }
}

/// SD 卡挂载结果
#[derive(Debug, Clone, Copy)]
pub struct SdMountInfo {
  /// SD 卡总容量（字节）
  pub bytes: u64,
  /// 根目录可枚举的条目数
  pub entries: u32,
}

/// 打印挂载成功信息到 defmt
pub fn log_mount(info: &SdMountInfo) {
  info!(
    "SD mounted: {} bytes, root entries: {}",
    info.bytes, info.entries
  );
}

/// 打印挂载失败到 defmt
pub fn log_mount_err<E: core::fmt::Debug>(err: &embedded_sdmmc::Error<E>) {
  warn!("SD mount failed: {:?}", defmt::Debug2Format(err));
}

/// 尝试探测并挂载 SD 卡，返回自检状态与（可选的）挂载信息。
///
/// 流程：`SdCard::num_bytes` → `VolumeManager::open_volume` → 枚举根目录。
/// 任一环节失败都会返回一个带简短原因的 [`SelfTestStatus::Fail`]，日志用 defmt 打出细节。
///
/// SD 卡是**可选外设**：调用方拿到 `Fail(..)` 可以选择继续启动，只在自检页警示即可。
pub fn try_mount<SPI, D>(spi: SPI, delay: D) -> (SelfTestStatus, Option<SdMountInfo>)
where
  SPI: SpiDevice,
  D: DelayNs,
{
  let sd = embedded_sdmmc::SdCard::new(spi, delay);
  let bytes = match sd.num_bytes() {
    Ok(b) => {
      info!("SD card detected: {} bytes", b);
      b
    }
    Err(err) => {
      warn!("SD init failed: {:?}", defmt::Debug2Format(&err));
      return (SelfTestStatus::Fail("no card"), None);
    }
  };

  let vm = embedded_sdmmc::VolumeManager::new(sd, DummyTimeSource);
  let volume = match vm.open_volume(embedded_sdmmc::VolumeIdx(0)) {
    Ok(v) => v,
    Err(err) => {
      log_mount_err(&err);
      return (SelfTestStatus::Fail("no FAT"), None);
    }
  };

  let mut entries: u32 = 0;
  if let Ok(root) = volume.open_root_dir() {
    let _ = root.iterate_dir(|_e| {
      entries = entries.saturating_add(1);
    });
  }

  let info = SdMountInfo { bytes, entries };
  log_mount(&info);
  (SelfTestStatus::Ok, Some(info))
}
