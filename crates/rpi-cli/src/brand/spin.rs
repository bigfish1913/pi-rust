//! 标志自转的时钟：开机转一圈，缓出后停在一个能看见厚度的 3/4 视角。
//!
//! 角度由「距片头开始的墙上时间」决定，所以不需要把状态塞进渲染器里；
//! 任何一次重绘都会算出当前该显示的角度。

use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 片头动画时长（秒）。
pub const INTRO_SECS: f32 = 7.0;

/// 停下来的自转角（弧度）：转完一整圈正好回到正面。
pub const REST_ANGLE: f32 = 0.0;

/// 是否允许播放动画。regular 模式写的是终端 scrollback，逐帧重绘会刷屏。
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 片头起点。用 `Mutex` 而不是原子时间戳，`replay` 只需要写一次。
static ORIGIN: OnceLock<Mutex<Instant>> = OnceLock::new();

/// 开关动画。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// 从头播放片头（欢迎页重建时调用）。
pub fn replay() {
    *origin().lock().unwrap() = Instant::now();
}

/// 片头是否还在播放。
pub fn animating() -> bool {
    ENABLED.load(Ordering::Relaxed) && elapsed() < INTRO_SECS
}

/// 当前自转角（弧度）。
pub fn angle() -> f32 {
    // 测试里固定停在一个角度：墙上时间会让同一测试里的两次渲染结果不同。
    #[cfg(test)]
    {
        REST_ANGLE
    }
    #[cfg(not(test))]
    {
        if !ENABLED.load(Ordering::Relaxed) {
            // regular 模式不播动画，只出静态的 3/4 视角。
            return REST_ANGLE;
        }
        angle_at(elapsed())
    }
}

/// 根据片头已播放的时间求自转角。
///
/// 用三次缓出把一整圈加一点余量均匀地铺开，最后停在 [`REST_ANGLE`]。
pub fn angle_at(elapsed: f32) -> f32 {
    if elapsed >= INTRO_SECS {
        return REST_ANGLE;
    }
    let p = (elapsed / INTRO_SECS).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - p).powi(3);
    (eased * (TAU + REST_ANGLE)) % TAU
}

fn origin() -> &'static Mutex<Instant> {
    ORIGIN.get_or_init(|| Mutex::new(Instant::now()))
}

fn elapsed() -> f32 {
    origin().lock().unwrap().elapsed().as_secs_f32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_face_on_and_ends_at_rest() {
        assert!(angle_at(0.0).abs() < 1e-4);
        assert!((angle_at(INTRO_SECS + 1.0) - REST_ANGLE).abs() < 1e-4);
        assert!((angle_at(INTRO_SECS) - REST_ANGLE).abs() < 1e-4);
    }

    #[test]
    fn angle_only_grows_during_intro() {
        let mut previous = angle_at(0.0);
        let mut wraps = 0;
        for step in 1..=70 {
            let a = angle_at(INTRO_SECS * step as f32 / 70.0);
            if a < previous - 1e-3 {
                // 转满一圈时必然回绕一次，其余位置不允许倒退。
                assert!(previous - a > 5.0, "非回绕的角度回退：{previous} -> {a}");
                wraps += 1;
            }
            previous = a;
        }
        assert_eq!(wraps, 1, "片头应恰好回绕一次");
    }

    #[test]
    fn enable_toggle_gates_animating() {
        set_enabled(false);
        assert!(!animating());
        set_enabled(true);
    }
}
