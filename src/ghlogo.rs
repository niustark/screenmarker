//! 「关于」窗口上的 GitHub 图标：官方 Octocat 矢量路径，由 GDI+ 直接填充。
//! 运行时不需要 png / ico 等任何图片资源，任意 DPI 下都是清晰的矢量边缘。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::GdiPlus::{
    FillModeWinding, GdipAddPathBezier, GdipClosePathFigure, GdipCreatePath,
    GdipCreateSolidFill, GdipDeleteBrush, GdipDeletePath, GdipFillPath, GdipSetPixelOffsetMode,
    GdipSetSmoothingMode, GpBrush, GpGraphics, GpPath, GpSolidFill, PixelOffsetModeHighQuality,
    SmoothingModeAntiAlias,
};

/// 官方 octicons mark-github-16.svg 的 path，转成 28 段首尾相连的三次贝塞尔。
/// 每段 8 个数：起点 x,y / 控制点 1 x,y / 控制点 2 x,y / 终点 x,y。
/// 坐标基于 16×16 viewBox；弧线命令已按标准算法展开为贝塞尔。
const BEZIERS: &[[f32; 8]] = &[
    [6.7660, 11.3280, 4.7030, 11.0780, 3.2500, 9.5940, 3.2500, 7.6720],
    [3.2500, 7.6720, 3.2500, 6.8910, 3.5310, 6.0470, 4.0000, 5.4840],
    [4.0000, 5.4840, 3.7970, 4.9690, 3.8280, 3.8750, 4.0630, 3.4220],
    [4.0630, 3.4220, 4.6880, 3.3440, 5.5310, 3.6720, 6.0310, 4.1250],
    [6.0310, 4.1250, 6.6250, 3.9380, 7.2500, 3.8440, 8.0160, 3.8440],
    [8.0160, 3.8440, 8.7810, 3.8440, 9.4060, 3.9380, 9.9690, 4.1090],
    [9.9690, 4.1090, 10.4530, 3.6720, 11.3130, 3.3440, 11.9380, 3.4220],
    [11.9380, 3.4220, 12.1560, 3.8440, 12.1880, 4.9370, 11.9840, 5.4690],
    [11.9840, 5.4690, 12.4840, 6.0620, 12.7500, 6.8590, 12.7500, 7.6720],
    [12.7500, 7.6720, 12.7500, 9.5940, 11.2970, 11.0470, 9.2030, 11.3120],
    [9.2030, 11.3120, 9.7340, 11.6560, 10.0930, 12.4060, 10.0930, 13.2660],
    [10.0930, 13.2660, 10.0930, 13.8077, 10.0930, 14.3493, 10.0930, 14.8910],
    [10.0930, 14.8910, 10.0930, 15.3590, 10.4840, 15.6250, 10.9530, 15.4380],
    [10.9530, 15.4380, 13.7810, 14.3590, 16.0000, 11.5300, 16.0000, 8.0300],
    [16.0000, 8.0300, 16.0000, 3.6100, 12.4060, 0.0000, 7.9840, 0.0000],
    [7.9840, 0.0000, 3.5630, 0.0000, 0.0000, 3.6100, 0.0000, 8.0310],
    [0.0000, 8.0310, -0.0092, 11.3467, 2.0582, 14.3135, 5.1720, 15.4530],
    [5.1720, 15.4530, 5.5940, 15.6090, 6.0000, 15.3280, 6.0000, 14.9060],
    [6.0000, 14.9060, 6.0000, 14.4893, 6.0000, 14.0727, 6.0000, 13.6560],
    [6.0000, 13.6560, 5.7810, 13.7500, 5.5000, 13.8120, 5.2500, 13.8120],
    [5.2500, 13.8120, 4.2190, 13.8120, 3.6100, 13.2500, 3.1720, 12.2030],
    [3.1720, 12.2030, 3.0000, 11.7810, 2.8120, 11.5310, 2.4530, 11.4840],
    [2.4530, 11.4840, 2.2660, 11.4690, 2.2030, 11.3910, 2.2030, 11.2970],
    [2.2030, 11.2970, 2.2030, 11.1090, 2.5160, 10.9690, 2.8280, 10.9690],
    [2.8280, 10.9690, 3.2810, 10.9690, 3.6720, 11.2500, 4.0780, 11.8290],
    [4.0780, 11.8290, 4.3910, 12.2810, 4.7180, 12.4840, 5.1090, 12.4840],
    [5.1090, 12.4840, 5.5000, 12.4840, 5.7500, 12.3440, 6.1090, 11.9840],
    [6.1090, 11.9840, 6.3750, 11.7190, 6.5790, 11.4840, 6.7660, 11.3280],
];

/// 把图标填进 r（等比缩放、居中）。调用方需已初始化 GDI+ 并创建好 GpGraphics。
pub unsafe fn draw(g: *mut GpGraphics, r: RECT) {
    if g.is_null() {
        return;
    }
    let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let _ = GdipSetPixelOffsetMode(g, PixelOffsetModeHighQuality);

    let mut path: *mut GpPath = std::ptr::null_mut();
    if GdipCreatePath(FillModeWinding, &mut path).0 != 0 || path.is_null() {
        return;
    }

    let w = (r.right - r.left) as f32;
    let h = (r.bottom - r.top) as f32;
    let s = w.min(h) / 16.0;
    let ox = r.left as f32 + (w - 16.0 * s) / 2.0;
    let oy = r.top as f32 + (h - 16.0 * s) / 2.0;
    for b in BEZIERS {
        let _ = GdipAddPathBezier(
            path,
            ox + b[0] * s,
            oy + b[1] * s,
            ox + b[2] * s,
            oy + b[3] * s,
            ox + b[4] * s,
            oy + b[5] * s,
            ox + b[6] * s,
            oy + b[7] * s,
        );
    }
    let _ = GdipClosePathFigure(path);

    // 原 PNG 实心部分的灰度为 1，沿用同一颜色保持视觉一致。
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    if GdipCreateSolidFill(0xFF01_0101, &mut brush).0 == 0 && !brush.is_null() {
        let _ = GdipFillPath(g, brush as *mut GpBrush, path);
        let _ = GdipDeleteBrush(brush as *mut GpBrush);
    }
    let _ = GdipDeletePath(path);
}
