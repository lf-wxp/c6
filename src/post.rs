//! Power-On Self-Test (POST) 驱动 helper
//!
//! 抽走 `main.rs` 里 6 次重复出现的 `mark → render → sleep` 模板：
//! ```ignore
//! report.mark(item, status);
//! let _ = render_self_test(display, &report);
//! Timer::after(Duration::from_millis(120)).await;
//! ```
//!
//! 用法：
//! ```ignore
//! post::step(&mut display, &mut report, SelfTestItem::Heap, run_heap_check()).await;
//! ```

use defmt::warn;
use embassy_time::{Duration, Timer};
use embedded_graphics::{pixelcolor::Rgb565, prelude::DrawTarget};

use crate::display::render_self_test;
use crate::self_test::{SelfTestItem, SelfTestReport, SelfTestStatus};

/// POST 每步之间的停顿：既让用户能看清进度，也给下一项外设一点复位时间
pub const STEP_DELAY: Duration = Duration::from_millis(120);

/// 在报告上标记一个自检项的结果，立即重画自检页，然后等待 [`STEP_DELAY`]。
///
/// 渲染失败不会阻断流程，只会打一条 warn 日志。
pub async fn step<D>(
  display: &mut D,
  report: &mut SelfTestReport,
  item: SelfTestItem,
  status: SelfTestStatus,
) where
  D: DrawTarget<Color = Rgb565>,
{
  report.mark(item, status);
  if render_self_test(display, report).is_err() {
    warn!("render_self_test error");
  }
  Timer::after(STEP_DELAY).await;
}

/// 只重画自检页并 sleep，不改动报告（用于首屏"全部 pending"）。
pub async fn refresh<D>(display: &mut D, report: &SelfTestReport)
where
  D: DrawTarget<Color = Rgb565>,
{
  if render_self_test(display, report).is_err() {
    warn!("render_self_test error");
  }
}
