"""生成 SubLink 应用图标。

设计意图：正方形圆角底 + 渐变，“重定向”语义用一个从文件夹转向内存条的箭头表达。
不依赖外部素材，纯 PIL 图元绘制，随时可重跑（尺寸多档输出，供 Windows ICO 与
Tauri 打包使用）。

用法：
    python scripts/make_icon.py            # 输出到 crates/sl-gui/icons/
    python scripts/make_icon.py <输出目录>
"""

from __future__ import annotations

import math
import os
import sys

from PIL import Image, ImageDraw

# 品牌色（深蓝 → 亮蓝），与应用内主题一致
BG_TOP = (37, 99, 235)      # #2563EB
BG_BOTTOM = (23, 42, 92)    # #172A5C
INK = (245, 248, 255)       # 前景（近白）
ACCENT = (56, 189, 248)     # #38BDF8 强调色

# ICO 需要包含的尺寸（Windows 会按场景挑最合适的一档）
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
SUPERSAMPLE = 4  # 先放大绘制再缩小，得到平滑边缘


def vertical_gradient(size: int, top: tuple[int, int, int], bottom: tuple[int, int, int]) -> Image.Image:
    grad = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / max(size - 1, 1)
        grad.putpixel(
            (0, y),
            tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3)),
        )
    return grad.resize((size, size))


def rounded_mask(size: int, radius_ratio: float = 0.22) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(mask)
    r = round(size * radius_ratio)
    d.rounded_rectangle((0, 0, size - 1, size - 1), radius=r, fill=255)
    return mask


def draw_glyph(img: Image.Image) -> None:
    """在已铺好底色的图上画前景：文件夹 + 转向内存条的箭头。"""
    s = img.width
    d = ImageDraw.Draw(img)

    # --- 文件夹（左侧）---
    fx0, fy0 = s * 0.14, s * 0.30
    fx1, fy1 = s * 0.56, s * 0.74
    tab_h = s * 0.075
    tab_w = s * 0.20
    lw = max(round(s * 0.045), 1)

    # 文件夹轮廓（含左上角的标签块）
    d.rounded_rectangle(
        (fx0, fy0 + tab_h, fx1, fy1),
        radius=s * 0.05,
        outline=INK,
        width=lw,
    )
    d.rounded_rectangle(
        (fx0, fy0, fx0 + tab_w, fy0 + tab_h * 2.1),
        radius=s * 0.035,
        fill=INK,
    )

    # --- 内存条（右侧）---
    mx0, my0 = s * 0.62, s * 0.38
    mx1, my1 = s * 0.88, s * 0.68
    d.rounded_rectangle((mx0, my0, mx1, my1), radius=s * 0.045, outline=ACCENT, width=lw)
    # 内存条上的“金手指”
    for i in range(3):
        x = mx0 + (mx1 - mx0) * (0.25 + 0.25 * i)
        d.line((x, my0 + lw * 0.8, x, my0 + (my1 - my0) * 0.30), fill=ACCENT, width=max(round(lw * 0.7), 1))

    # --- 从文件夹指向内存条的箭头 ---
    ax0 = fx1 + s * 0.005
    ax1 = mx0 - s * 0.02
    ay = (fy0 + fy1) / 2
    d.line((ax0, ay, ax1, ay), fill=INK, width=lw)
    head = s * 0.055
    d.polygon(
        [(ax1 + head * 0.75, ay), (ax1 - head * 0.2, ay - head), (ax1 - head * 0.2, ay + head)],
        fill=INK,
    )


def render_master(size: int = 512) -> Image.Image:
    big = size * SUPERSAMPLE
    bg = vertical_gradient(big, BG_TOP, BG_BOTTOM).convert("RGBA")
    glyph = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    draw_glyph(glyph)
    bg.alpha_composite(glyph)
    bg.putalpha(rounded_mask(big))
    return bg.resize((size, size), Image.LANCZOS)


def square_icon(size: int) -> Image.Image:
    """给 ICO 用：小尺寸下不做圆角（Windows 自己会处理），避免 16px 时糊成一团。"""
    master = render_master(max(size * SUPERSAMPLE, 256))
    img = master.resize((size, size), Image.LANCZOS).convert("RGBA")
    if size <= 24:
        # 极小尺寸只保留剪刀面：单色底 + 简化的箭头，保证可辨识
        flat = Image.new("RGBA", (size, size), BG_TOP + (255,))
        d = ImageDraw.Draw(flat)
        pad = max(size * 0.18, 1)
        lw = max(round(size * 0.11), 1)
        d.line((pad, size / 2, size - pad, size / 2), fill=INK, width=lw)
        d.polygon(
            [(size - pad + lw, size / 2), (size * 0.55, pad), (size * 0.55, size - pad)],
            fill=INK,
        )
        return flat
    return img


def main() -> int:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(root, "crates", "sl-gui", "icons")
    os.makedirs(out_dir, exist_ok=True)

    master = render_master(512)
    png_path = os.path.join(out_dir, "icon.png")
    master.save(png_path)
    print(f"写出 {png_path}  ({master.width}x{master.height})")

    # Tauri 打包也会用到这两档 PNG
    for size, name in ((32, "32x32.png"), (128, "128x128.png")):
        p = os.path.join(out_dir, name)
        master.resize((size, size), Image.LANCZOS).save(p)
        print(f"写出 {p}")

    frames = [square_icon(s) for s in ICO_SIZES]
    ico_path = os.path.join(out_dir, "icon.ico")
    frames[-1].save(
        ico_path,
        format="ICO",
        sizes=[(f.width, f.height) for f in frames],
        append_images=frames[:-1],
    )
    print(f"写出 {ico_path}  ({', '.join(str(s) for s in ICO_SIZES)})")

    # 自检：四角必须是透明的（圆角），否则说明遮罩没生效
    corners = [
        master.getpixel((0, 0)),
        master.getpixel((master.width - 1, 0)),
        master.getpixel((0, master.height - 1)),
        master.getpixel((master.width - 1, master.height - 1)),
    ]
    alphas = [c[3] for c in corners]
    if any(a != 0 for a in alphas):
        print(f"警告：四角 alpha 非 0：{alphas}", file=sys.stderr)
        return 2
    print(f"自检通过：四角透明 alpha={alphas}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
