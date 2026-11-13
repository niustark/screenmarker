//! 「关于」窗口上的网站图标：一支斜置的铅笔（app.ico 的主体图形）。
//!
//! 与 ghlogo.rs 同理，用 GDI+ 的多边形路径直接填充，运行时不需要任何图片资源，
//! 任意 DPI 下都是清晰的矢量边缘。
//!
//! 几何来源：app.ico 256×256 帧里量出的色块角点（铅笔长轴沿「右上→左下」斜放）：
//!   - 粉尾（橡皮擦） 平行四边形 (176,36)(129,83)(172,126)(219,79)
//!   - 红身（笔杆）   平行四边形 (129,83)(62,150)(105,193)(172,126)
//!   - 黑尖（笔芯）   三角形       (62,150)(105,193)(37.5,217.5)
//! 三段沿对角线首尾相接，分界处留白即原图的白色细缝（不额外画线）。

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::GdiPlus::{
    FillModeWinding, GdipAddPathPolygon, GdipCreatePath, GdipCreateSolidFill, GdipDeleteBrush,
    GdipDeletePath, GdipFillPath, GdipSetPixelOffsetMode, GdipSetSmoothingMode, GpBrush,
    GpGraphics, GpPath, GpSolidFill, PixelOffsetModeHighQuality, SmoothingModeAntiAlias, PointF,
};

/// 每个色块一个闭合多边形；坐标基于 256×256、y 向下，绘制时等比缩放到目标矩形。
/// 每组：首元素是 ARGB 颜色，后面是顶点列表。
const BLOCKS: &[(u32, &[(f32, f32)])] = &[
    // 粉尾（橡皮擦）
    (
        0xFFF2_C4C8,
        &[(176.0, 36.0), (129.0, 83.0), (172.0, 126.0), (219.0, 79.0)],
    ),
    // 红身（笔杆）
    (
        0xFFFF_4D4F,
        &[(129.0, 83.0), (62.0, 150.0), (105.0, 193.0), (172.0, 126.0)],
    ),
    // 黑尖（笔芯）
    (
        0xFF2B_2B2B,
        &[(62.0, 150.0), (105.0, 193.0), (37.5, 217.5)],
    ),
];

/// 把铅笔填进 r（等比缩放、居中）。调用方需已初始化 GDI+ 并创建好 GpGraphics。
pub unsafe fn draw(g: *mut GpGraphics, r: RECT) {
    if g.is_null() {
        return;
    }
    let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let _ = GdipSetPixelOffsetMode(g, PixelOffsetModeHighQuality);

    let w = (r.right - r.left) as f32;
    let h = (r.bottom - r.top) as f32;
    // 铅笔原始包围盒：x∈[16,219]、y∈[36,218]，接近 256×256 的全图，
    // 直接按 256 等比缩放并居中即可。
    let s = w.min(h) / 256.0;
    let ox = r.left as f32 + (w - 256.0 * s) / 2.0;
    let oy = r.top as f32 + (h - 256.0 * s) / 2.0;

    for (argb, pts) in BLOCKS {
        let mut path: *mut GpPath = std::ptr::null_mut();
        if GdipCreatePath(FillModeWinding, &mut path).0 != 0 || path.is_null() {
            continue;
        }
        let mut poly: Vec<PointF> = pts
            .iter()
            .map(|(x, y)| PointF {
                X: ox + x * s,
                Y: oy + y * s,
            })
            .collect();
        let _ = GdipAddPathPolygon(path, poly.as_mut_ptr(), poly.len() as i32);

        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        if GdipCreateSolidFill(*argb, &mut brush).0 == 0 && !brush.is_null() {
            let _ = GdipFillPath(g, brush as *mut GpBrush, path);
            let _ = GdipDeleteBrush(brush as *mut GpBrush);
        }
        let _ = GdipDeletePath(path);
    }
}
