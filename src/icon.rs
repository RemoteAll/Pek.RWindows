//! 程序/托盘图标：纯 `std` 绘制（不引入图像库），`build.rs` 生成 `.ico` 与运行期托盘图标共用。
//!
//! 设计：蓝色渐变圆角方块 + 白色放大镜（搜索的通用隐喻）。

/// 生成 `size × size` 的 RGBA 像素（未预乘 alpha，行优先）。
pub fn draw_rgba(size: u32) -> Vec<u8> {
    let n = size.max(8);
    // 3×3 超采样，边缘抗锯齿（不依赖任何绘图库）
    const SS: u32 = 3;
    let mut out = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) / n as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) / n as f32;
                    let c = sample(px, py);
                    for i in 0..4 {
                        acc[i] += c[i];
                    }
                }
            }
            let cnt = (SS * SS) as f32;
            let idx = ((y * n + x) * 4) as usize;
            for i in 0..4 {
                out[idx + i] = (acc[i] / cnt).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// 写入多尺寸 Windows `.ico`（BMP 编码，兼容从 Win7 到 Win11 的所有壳层）。
///
/// 主程序不使用：由 `build.rs` 调用生成 exe 图标；保留在共享模块里以免两处维护绘制代码。
#[allow(dead_code)]
pub fn write_ico(path: &std::path::Path, sizes: &[u32]) -> std::io::Result<()> {
    let images: Vec<(u32, Vec<u8>)> = sizes.iter().map(|&s| (s, draw_rgba(s))).collect();
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6u32 + 16u32 * images.len() as u32;
    for (size, _) in &images {
        let n = *size;
        let mask_row = n.div_ceil(32) * 4;
        let bmp_size = 40 + n * n * 4 + mask_row * n;
        out.push(if n >= 256 { 0 } else { n as u8 });
        out.push(if n >= 256 { 0 } else { n as u8 });
        out.push(0); // 调色板数
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&bmp_size.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += bmp_size;
    }

    for (size, rgba) in &images {
        let n = *size;
        let mask_row = n.div_ceil(32) * 4;
        // BITMAPINFOHEADER：biHeight 取 2×n（像素 + AND 掩码）
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(n as i32).to_le_bytes());
        out.extend_from_slice(&((n as i32) * 2).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&(n * n * 4).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]); // 分辨率 / 调色板信息
        // 像素：自下而上，BGRA
        for y in (0..n).rev() {
            for x in 0..n {
                let i = ((y * n + x) * 4) as usize;
                out.push(rgba[i + 2]);
                out.push(rgba[i + 1]);
                out.push(rgba[i]);
                out.push(rgba[i + 3]);
            }
        }
        // AND 掩码（32bpp 下不参与，但要占位）
        out.extend_from_slice(&vec![0u8; (mask_row * n) as usize]);
    }

    std::fs::write(path, out)
}

/// 单像素采样：圆角方块底 + 白色放大镜。
fn sample(px: f32, py: f32) -> [f32; 4] {
    const M: f32 = 0.035; // 外边距
    if !inside_round_rect(px - M, py - M, 1.0 - 2.0 * M, 1.0 - 2.0 * M, 0.24) {
        return [0.0, 0.0, 0.0, 0.0];
    }

    // 竖向渐变（顶部亮蓝 → 底部深蓝）
    let t = py.clamp(0.0, 1.0);
    let mut c = [
        lerp(84.0, 38.0, t),
        lerp(146.0, 92.0, t),
        lerp(255.0, 214.0, t),
        255.0,
    ];

    // 放大镜镜片（圆环）
    let lens = (0.425f32, 0.415f32);
    let ring_r = 0.175f32;
    let ring_w = 0.075f32;
    let (dx, dy) = (px - lens.0, py - lens.1);
    let d_lens = (dx * dx + dy * dy).sqrt();
    let d_ring = (d_lens - ring_r).abs() - ring_w * 0.5;

    // 放大镜手柄（圆头线段）
    let d_handle = dist_to_segment((px, py), (0.552, 0.545), (0.745, 0.738)) - 0.052;

    if d_ring <= 0.0 || d_handle <= 0.0 {
        c = [255.0, 255.0, 255.0, 255.0];
    }
    c
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn inside_round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    if x < 0.0 || y < 0.0 || x > w || y > h {
        return false;
    }
    let (cx, cy) = (x.clamp(r, w - r), y.clamp(r, h - r));
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

fn dist_to_segment(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let (vx, vy) = (bx - ax, by - ay);
    let (wx, wy) = (px - ax, py - ay);
    let len2 = vx * vx + vy * vy;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    };
    let (dx, dy) = (wx - vx * t, wy - vy * t);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_are_sane() {
        let px = draw_rgba(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(px[center + 3], 255, "中心像素应不透明");
        // 四角应透明（圆角方块）
        assert_eq!(px[3], 0, "左上角应透明");
    }

    #[test]
    fn ico_header_is_valid() {
        let dir = std::env::temp_dir().join("pek-rwindows-icon-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.ico");
        write_ico(&path, &[16, 32]).unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], &[0, 0, 1, 0]);
        assert_eq!(u16::from_le_bytes([data[4], data[5]]), 2);
        assert!(data.len() > 100);
        let _ = std::fs::remove_file(&path);
    }
}
