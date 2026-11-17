//! 把二维轮廓挤出成 3D 实体并光栅化成终端字符。
//!
//! 相机是正交投影，沿视线方向按固定步长扫描；物体绕垂直中轴自转，
//! 相机俯视 12°。每个字符格子做 2×2 子采样，覆盖率决定字符密度，
//! 命中点的法线决定明暗档位（也就是主题里的哪一档颜色）。

use rpi_tui::theme as current_theme;

use super::shape;

/// 输出列数。
pub const COLS: usize = 44;
/// 输出行数，也就是欢迎页里占的终端高度。
pub const ROWS: usize = 22;
/// 每个字符格子的子采样精度（2×2 抗锯齿）。
const SS: usize = 2;
/// 世界半宽：越大 logo 越小、四周留白越多。
const WORLD: f32 = 1.30;
/// 盘子厚度（齿顶半径 = 1.0）。
const THICK: f32 = 0.18;
/// 沿视线方向的扫描步长。
const COARSE: f32 = 0.025;
/// 覆盖率低于这个值的格子留空。
const MIN_COVER: f32 = 0.22;
/// 字符亮度梯度。
const RAMP: [char; 10] = [' ', '.', ':', '-', '=', '+', '*', '#', '%', '@'];

/// 把标志渲染成 [`ROWS`] 行、每行 [`COLS`] 个单宽字符，并套上当前主题的颜色。
pub fn lockup(yaw: f32) -> Vec<String> {
    let mut cov = vec![0.0f32; COLS * ROWS];
    let mut shade = vec![0.0f32; COLS * ROWS];
    raster(yaw, &mut cov, &mut shade);
    compose(&cov, &shade)
}

/// 正交相机 + 当前自转角。
///
/// 世界坐标到物体坐标的变换只有一份实现，`march` 和 `shade_at` 共用，
/// 以后要改视角或换成透视投影都只需要动这里。
struct Camera {
    st: f32,
    ct: f32,
    sp: f32,
    cp: f32,
}

impl Camera {
    /// 相机俯角。
    const PITCH: f32 = 12.0 * std::f32::consts::PI / 180.0;

    fn at(yaw: f32) -> Self {
        let (st, ct) = yaw.sin_cos();
        let (sp, cp) = Self::PITCH.sin_cos();
        Self { st, ct, sp, cp }
    }

    /// 世界坐标 -> 物体坐标：先绕 Y 轴自转，再按相机俯角绕 X 轴旋转。
    #[inline]
    fn to_object(&self, x: f32, y: f32, z: f32) -> (f32, f32, f32) {
        let x1 = x * self.ct - z * self.st;
        let z1 = x * self.st + z * self.ct;
        let y2 = y * self.cp + z1 * self.sp;
        let z2 = -y * self.sp + z1 * self.cp;
        (x1, y2, z2)
    }

    /// 沿视线从 `zr` 往里扫描，返回第一个落在实体内部的 z。
    ///
    /// 实体 = 齿轮盘（有厚度）减去镂空的 rpi 笔画；命中即停，背景点扫完包围盒就放弃。
    #[inline]
    fn march(&self, px: f32, py: f32, zr: f32, steps: usize) -> Option<f32> {
        let mut z = zr;
        for _ in 0..steps {
            let (x, y, z2) = self.to_object(px, py, z);
            if z2.abs() <= THICK && shape::disc(x, y) <= 0.0 && shape::mark(x, y) > 0.0 {
                return Some(z);
            }
            z -= COARSE;
        }
        None
    }

    /// 命中点的法线光照，返回 0..1 的亮度。
    #[inline]
    fn shade_at(&self, px: f32, py: f32, z: f32) -> f32 {
        let e = 0.008f32;
        // 世界坐标下的有符号距离，用来做中心差分求法线。
        let at = |x: f32, y: f32, zz: f32| {
            let (x1, y2, z2) = self.to_object(x, y, zz);
            shape::disc(x1, y2).max(z2.abs() - THICK)
        };
        let nx = at(px + e, py, z) - at(px - e, py, z);
        let ny = at(px, py + e, z) - at(px, py - e, z);
        let nz = at(px, py, z + e) - at(px, py, z - e);
        let len = (nx * nx + ny * ny + nz * nz).sqrt() + 1e-9;
        let (nx, ny, nz) = (nx / len, ny / len, nz / len);
        // 主光在左上前方。
        let diff = (nx * -0.4025 + ny * 0.5031 + nz * 0.7647).max(0.0);
        let mut s = 0.32 + 0.55 * diff;
        if nz.abs() < 0.35 {
            // 侧壁压暗，厚度才看得出来。
            s *= 0.80;
        }
        s
    }
}

/// 光栅化：对每个子采样点做一次沿视线的扫描，记录覆盖率和亮度。
fn raster(yaw: f32, cov: &mut [f32], shade: &mut [f32]) {
    let cam = Camera::at(yaw);
    // 物体包围盒在视线方向上的半宽：越接近侧视，z 跨度越大。
    let reach = WORLD + 0.05;
    let zr = cam.sp.abs() * reach
        + (cam.st * cam.cp).abs() * reach
        + (cam.ct * cam.cp).abs() * THICK
        + 0.03;
    let steps = (2.0 * zr / COARSE) as usize + 1;

    for j in 0..ROWS {
        for i in 0..COLS {
            let mut hits = 0u32;
            let mut shade_sum = 0.0f32;
            for sj in 0..SS {
                for si in 0..SS {
                    // 子采样点落在格子内部，偏移半个子采样宽度。
                    let px = -WORLD
                        + 2.0 * WORLD * (i * SS + si) as f32 / (COLS * SS) as f32
                        + WORLD / (COLS * SS) as f32;
                    let py = WORLD
                        - 2.0 * WORLD * (j * SS + sj) as f32 / (ROWS * SS) as f32
                        - WORLD / (ROWS * SS) as f32;
                    if let Some(z) = cam.march(px, py, zr, steps) {
                        hits += 1;
                        shade_sum += cam.shade_at(px, py, z);
                    }
                }
            }
            let k = j * COLS + i;
            if hits > 0 {
                cov[k] = hits as f32 / (SS * SS) as f32;
                shade[k] = shade_sum / hits as f32;
            }
        }
    }
}

/// 把覆盖率 + 亮度变成带颜色的文本行。
fn compose(cov: &[f32], shade: &[f32]) -> Vec<String> {
    let colors = current_theme().colors;
    let palette = [colors.dim, colors.muted, colors.accent, colors.text];
    let mut lines = Vec::with_capacity(ROWS);
    for j in 0..ROWS {
        let mut line = String::new();
        let mut i = 0;
        while i < COLS {
            let k = j * COLS + i;
            if cov[k] < MIN_COVER {
                line.push(' ');
                i += 1;
                continue;
            }
            // 同一颜色档的连续格子合成一次转义，避免每帧产生大量序列。
            let level = level_of(shade[k]);
            let start = i;
            while i < COLS {
                let k = j * COLS + i;
                if cov[k] < MIN_COVER || level_of(shade[k]) != level {
                    break;
                }
                i += 1;
            }
            let run: String = (start..i)
                .map(|c| {
                    let k = j * COLS + c;
                    let b = (shade[k] * (0.82 + 0.18 * cov[k])).clamp(0.0, 1.0);
                    RAMP[(b * 9.0 + 0.5) as usize]
                })
                .collect();
            line.push_str(&palette[level].fg(&run));
        }
        lines.push(line);
    }
    lines
}

/// 亮度分档，决定用主题里的哪一档颜色。
#[inline]
fn level_of(s: f32) -> usize {
    if s < 0.34 {
        0
    } else if s < 0.60 {
        1
    } else if s < 0.82 {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::ansi::visible_width;

    #[test]
    fn lockup_has_stable_geometry() {
        let rows = lockup(0.0);
        assert_eq!(rows.len(), ROWS);
        for row in &rows {
            assert_eq!(visible_width(row), COLS);
        }
    }

    #[test]
    fn lockup_is_deterministic_for_a_given_angle() {
        assert_eq!(lockup(0.5), lockup(0.5));
    }

    #[test]
    fn lockup_changes_with_angle() {
        assert_ne!(lockup(0.0), lockup(0.9));
    }

    #[test]
    fn face_on_view_has_solid_ink() {
        let rows = lockup(0.0);
        // 正面应该能看到一片实心盘面，而不是几条稀疏的线。
        let inked: usize = rows
            .iter()
            .map(|row| {
                rpi_tui::strip_ansi(row)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count()
            })
            .sum();
        assert!(inked > 300, "正面实心像素只有 {inked} 个");
    }
}
