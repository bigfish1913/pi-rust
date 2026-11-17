//! 3D 标志的二维轮廓：实心齿轮盘 + 镂空的 rpi 图案。
//!
//! 所有几何都在「齿顶半径 = 1.0」的归一化坐标系里描述；
//! 渲染层负责把它沿 z 轴挤出成有厚度的实体再做投影和光照。

use std::f32::consts::TAU;

/// 齿轮齿数。
const TEETH: f32 = 14.0;
/// 齿顶半宽（按一个齿的周期归一化，越小齿越窄）。
const TOOTH_TOP: f32 = 0.20;
/// 齿根半宽。
const TOOTH_BASE: f32 = 0.36;
/// 齿根半径：齿从 0.82 长出 0.18 到齿顶 1.0。
const TOOTH_ROOT: f32 = 0.82;
/// 齿高。
const TOOTH_TALL: f32 = 0.18;
/// rpi 图案的笔画半宽。
const STROKE: f32 = 0.072;

/// 齿轮的径向齿形，返回 `0..=1`（0 = 齿谷，1 = 齿顶）。
#[inline]
pub fn tooth_profile(x: f32, y: f32) -> f32 {
    // 把极角映射到一个齿的周期上，取到齿中心的归一化距离。
    let u = (y.atan2(x) / TAU * TEETH).rem_euclid(1.0);
    let t = (u - 0.5).abs() * 2.0;
    if t <= TOOTH_TOP {
        1.0
    } else if t >= TOOTH_BASE {
        0.0
    } else {
        (TOOTH_BASE - t) / (TOOTH_BASE - TOOTH_TOP)
    }
}

/// 实心齿轮盘的二维有符号距离（< 0 表示落在盘内）。
#[inline]
pub fn disc(x: f32, y: f32) -> f32 {
    let r = (x * x + y * y).sqrt();
    r - (TOOTH_ROOT + TOOTH_TALL * tooth_profile(x, y))
}

/// 点到线段的最短距离减去半宽 `w`。
#[inline]
fn segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, w: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt() - w
}

/// rpi 图案的二维有符号距离（< 0 表示笔画内部，也就是要从盘面镂空掉的区域）。
///
/// 取自原图 `docs/images/rpi-logo.jpg`：大圆环是 `p` 的碗，中间套一个小圆，
/// 竖腿贯穿到底部并向右弯；左侧是 `r` 的弧和竖线，右侧是 `i` 的点。
#[inline]
pub fn mark(x: f32, y: f32) -> f32 {
    // p 的碗（大圆环）+ 碗内的小圆。
    let dy = y - 0.15;
    let r = (x * x + dy * dy).sqrt();
    let mut d = (r - 0.34).abs() - STROKE;
    d = d.min((r - 0.112).abs() - STROKE);
    // p 的竖腿，以及底部向右的一小段弯脚。
    d = d.min(segment(x, y, 0.0, 0.12, 0.0, -0.54, STROKE));
    d = d.min(segment(x, y, 0.0, -0.54, 0.19, -0.58, STROKE));
    // 左侧 r 的弧和竖线。
    d = d.min(segment(x, y, -0.50, 0.30, -0.575, 0.02, STROKE));
    d = d.min(segment(x, y, -0.575, 0.02, -0.575, -0.26, STROKE));
    // 右侧 i 的点。
    let dx = x - 0.485;
    let dy2 = y + 0.02;
    d.min((dx * dx + dy2 * dy2).sqrt() - 0.066)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_covers_the_tooth_ring() {
        // 齿谷半径 0.82 内侧、齿顶 1.0 外侧。
        assert!(disc(0.0, 0.78) < 0.0);
        assert!(disc(0.0, 0.0) < 0.0);
        assert!(disc(0.0, 1.05) > 0.0);
    }

    #[test]
    fn mark_hollows_the_p_bowl() {
        // p 碗的圆环中心线上是笔画内部。
        assert!(mark(0.0, 0.15 + 0.34) < 0.0);
        // 碗的空白区域不在笔画里。
        assert!(mark(0.0, 0.15 + 0.22) > 0.0);
        // i 的点。
        assert!(mark(0.485, -0.02) < 0.0);
    }

    #[test]
    fn tooth_profile_alternates() {
        let mut hi = 0;
        let mut lo = 0;
        for k in 0..100 {
            let a = k as f32 / 100.0 * TAU;
            let p = tooth_profile(a.cos(), a.sin());
            if p > 0.9 {
                hi += 1;
            }
            if p < 0.1 {
                lo += 1;
            }
        }
        assert!(hi > 20 && lo > 20, "齿顶 {hi} / 齿谷 {lo}");
    }
}
