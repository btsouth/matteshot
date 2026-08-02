"""Build the Matteshot Product Hunt gallery and demo video.

The source screenshots in captures/ come from Matteshot's own headless capture
and editor test paths. This script only composes those real captures into the
1270x760 artwork Product Hunt expects.
"""

from __future__ import annotations

import math
import subprocess
from functools import lru_cache
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont


ROOT = Path(__file__).resolve().parent
CAPTURES = ROOT / "captures"
OUT = ROOT / "output"
OUT.mkdir(parents=True, exist_ok=True)

W, H = 1270, 760
FONT_REGULAR = Path(r"C:\Windows\Fonts\segoeui.ttf")
FONT_SEMIBOLD = Path(r"C:\Windows\Fonts\seguisb.ttf")
FONT_BOLD = Path(r"C:\Windows\Fonts\segoeuib.ttf")


def font(size: int, weight: str = "regular") -> ImageFont.FreeTypeFont:
    path = {"regular": FONT_REGULAR, "semibold": FONT_SEMIBOLD, "bold": FONT_BOLD}[weight]
    try:
        return ImageFont.truetype(str(path), size)
    except OSError as error:
        raise RuntimeError(
            "Matteshot launch assets require the Windows Segoe UI regular, "
            "semibold, and bold font files under C:\\Windows\\Fonts."
        ) from error


def gradient(size=(W, H), left=(25, 32, 71), right=(39, 37, 99)) -> Image.Image:
    w, h = size
    image = Image.new("RGB", size)
    px = image.load()
    for x in range(w):
        t = x / max(1, w - 1)
        for y in range(h):
            glow = max(0.0, 1.0 - math.hypot((x - w * .83) / (w * .65), (y - h * .1) / (h * .8)))
            r = int(left[0] * (1 - t) + right[0] * t + 17 * glow)
            g = int(left[1] * (1 - t) + right[1] * t + 13 * glow)
            b = int(left[2] * (1 - t) + right[2] * t + 22 * glow)
            px[x, y] = (min(r, 255), min(g, 255), min(b, 255))
    haze = Image.new("RGBA", size, (0, 0, 0, 0))
    hd = ImageDraw.Draw(haze)
    hd.ellipse((-210, -250, 500, 460), fill=(65, 201, 190, 42))
    hd.ellipse((820, 300, 1450, 940), fill=(179, 85, 214, 44))
    haze = haze.filter(ImageFilter.GaussianBlur(100))
    return Image.alpha_composite(image.convert("RGBA"), haze)


@lru_cache(maxsize=8)
def cached_gradient(size=(W, H), left=(25, 32, 71), right=(39, 37, 99)) -> Image.Image:
    return gradient(size, left, right)


@lru_cache(maxsize=16)
def cached_image(path: str) -> Image.Image:
    with Image.open(path) as image:
        return image.convert("RGBA").copy()


def rounded(image: Image.Image, radius: int) -> Image.Image:
    image = image.convert("RGBA")
    mask = Image.new("L", image.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, image.width, image.height), radius=radius, fill=255)
    image.putalpha(mask)
    return image


def fit(image: Image.Image, box: tuple[int, int], contain: bool = True) -> Image.Image:
    target_w, target_h = box
    scale = min(target_w / image.width, target_h / image.height) if contain else max(target_w / image.width, target_h / image.height)
    size = (max(1, round(image.width * scale)), max(1, round(image.height * scale)))
    resized = image.resize(size, Image.Resampling.LANCZOS)
    if contain:
        return resized
    x = max(0, (resized.width - target_w) // 2)
    y = max(0, (resized.height - target_h) // 2)
    return resized.crop((x, y, x + target_w, y + target_h))


def paste_card(canvas: Image.Image, image: Image.Image, xy: tuple[int, int], box: tuple[int, int], radius=18, shadow=24, contain=True):
    image = fit(image, box, contain=contain)
    image = rounded(image, radius)
    x = xy[0] + (box[0] - image.width) // 2
    y = xy[1] + (box[1] - image.height) // 2
    layer = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    sd = ImageDraw.Draw(layer)
    sd.rounded_rectangle((x + 3, y + 12, x + image.width + 3, y + image.height + 12), radius=radius, fill=(4, 7, 20, 180))
    layer = layer.filter(ImageFilter.GaussianBlur(shadow))
    canvas.alpha_composite(layer)
    canvas.alpha_composite(image, (x, y))
    return (x, y, image.width, image.height)


def text(draw: ImageDraw.ImageDraw, xy, value, size, fill=(255, 255, 255), weight="regular", anchor=None, spacing=5):
    draw.multiline_text(xy, value, font=font(size, weight), fill=fill, anchor=anchor, spacing=spacing)


def pill(draw: ImageDraw.ImageDraw, xy, label: str, accent=(108, 209, 198)):
    f = font(16, "semibold")
    bbox = draw.textbbox((0, 0), label, font=f)
    width = bbox[2] - bbox[0] + 28
    height = 38
    x, y = xy
    draw.rounded_rectangle(
        (x, y, x + width, y + height),
        radius=19,
        fill=(32, 41, 77, 255),
        outline=(88, 105, 154, 255),
        width=1,
    )
    draw.ellipse((x + 12, y + 15, x + 20, y + 23), fill=accent)
    draw.text((x + 27, y + 9), label, font=f, fill=(235, 240, 255))
    return width


def brand(xy=(64, 44), dark=False):
    icon = Image.open(ROOT.parents[1] / "assets" / "icon-256.png").convert("RGBA").resize((44, 44), Image.Resampling.LANCZOS)
    return icon, (xy[0] + 58, xy[1] + 7), (20, 27, 43) if dark else (255, 255, 255)


def add_brand(canvas: Image.Image, xy=(64, 40), label="MATTESHOT"):
    draw = ImageDraw.Draw(canvas)
    icon, tx, color = brand(xy)
    canvas.alpha_composite(icon, xy)
    draw.text(tx, label, font=font(18, "bold"), fill=color)


def save(canvas: Image.Image, name: str):
    canvas.convert("RGB").save(OUT / name, quality=94, subsampling=0)


def slide_hero():
    canvas = gradient()
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (66, 150), "Press PrtScn.\nPick a look.\nPaste.", 55, weight="bold", spacing=0)
    text(draw, (69, 355), "Six polished results before\nan editor ever opens.", 24, fill=(197, 206, 231), spacing=7)
    x = 67
    for label in ("Native Windows", "Local processing", "$19 once"):
        x += pill(draw, (x, 486), label) + 10
    aurora = Image.open(CAPTURES / "demo" / "matte-aurora.png")
    paste_card(canvas, aurora, (515, 95), (690, 530), radius=19, shadow=26)
    draw.rounded_rectangle((815, 638, 1118, 688), radius=25, fill=(72, 189, 158))
    text(draw, (966, 663), "Copied. Ready to paste.", 17, weight="semibold", anchor="mm")
    save(canvas, "01-hero.jpg")


def slide_styles():
    canvas = gradient(left=(20, 27, 55), right=(29, 48, 86))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (64, 104), "One capture. Six finished looks.", 42, weight="bold")
    text(draw, (65, 162), "Automatic backgrounds, spacing and shadows. Click once and keep moving.", 19, fill=(196, 206, 229))
    names = ["Adaptive", "Deep", "Aurora", "Slate", "Paper", "Pop"]
    files = [f"matte-{name.lower()}.png" for name in names]
    for i, (name, file) in enumerate(zip(names, files)):
        col, row = i % 3, i // 3
        x, y = 64 + col * 398, 220 + row * 245
        img = Image.open(CAPTURES / "demo" / file)
        card = fit(img, (360, 198), contain=True)
        paste_card(canvas, card, (x, y), (360, 198), radius=13, shadow=14)
        draw.rounded_rectangle((x + 14, y + 168, x + 124, y + 202), radius=17, fill=(15, 20, 42, 220))
        text(draw, (x + 69, y + 185), name, 14, weight="semibold", anchor="mm")
    save(canvas, "02-six-looks.jpg")


def slide_editor():
    canvas = gradient(left=(19, 27, 51), right=(39, 40, 82))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (64, 131), "Edit only when\nyou need to.", 43, weight="bold", spacing=2)
    text(draw, (66, 252), "The fast path stays fast.\nThe useful tools are still here.", 21, fill=(193, 204, 229), spacing=7)
    y = 350
    for label in ("9 annotation tools", "Precise redaction", "Live output sizing", "Fast matte controls"):
        pill(draw, (66, y), label)
        y += 50
    editor = Image.open(CAPTURES / "editor" / "raw.png")
    paste_card(canvas, editor, (375, 76), (845, 620), radius=16, shadow=22)
    save(canvas, "03-photo-editor.jpg")


@lru_cache(maxsize=1)
def ocr_editor_preview() -> Image.Image:
    editor = Image.open(CAPTURES / "editor" / "raw.png").convert("RGBA")
    overlay = Image.new("RGBA", editor.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    # Match Matteshot's select-text treatment: recognized word boxes are faint,
    # while the words included in the current drag selection are more visible.
    word_boxes = [
        (502, 224, 644, 254), (655, 224, 776, 254), (502, 265, 557, 285),
        (566, 265, 580, 285), (587, 265, 675, 285), (682, 265, 757, 285),
        (765, 265, 832, 285), (839, 265, 915, 285), (922, 265, 966, 285),
        (521, 329, 604, 347), (521, 363, 555, 397),
        (787, 329, 900, 347), (787, 363, 854, 397),
        (1058, 329, 1179, 347), (1058, 363, 1127, 397),
        (1327, 329, 1435, 347), (1327, 363, 1374, 397),
        (570, 672, 637, 690), (648, 672, 697, 690), (708, 672, 777, 690),
        (570, 699, 660, 717), (668, 699, 711, 717), (720, 699, 744, 717),
        (752, 699, 810, 717), (818, 699, 872, 717),
    ]
    for rect in word_boxes:
        draw.rounded_rectangle(rect, radius=4, fill=(78, 149, 236, 34), outline=(102, 174, 255, 80), width=1)
    selected = [(570, 672, 637, 690), (648, 672, 697, 690), (708, 672, 777, 690),
                (570, 699, 660, 717), (668, 699, 711, 717), (720, 699, 744, 717),
                (752, 699, 810, 717), (818, 699, 872, 717)]
    for rect in selected:
        draw.rounded_rectangle(rect, radius=4, fill=(60, 132, 231, 105), outline=(110, 193, 255, 220), width=2)
    editor.alpha_composite(overlay)
    return editor


def slide_ocr():
    canvas = gradient(left=(18, 28, 57), right=(28, 54, 88))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (64, 123), "Select text.\nSkip retyping.", 45, weight="bold", spacing=2)
    text(draw, (66, 242), "Drag over words in the screenshot\nlike they were normal text.", 21, fill=(193, 204, 229), spacing=7)
    y = 345
    for label in ("Offline Windows OCR", "Copy only what you need", "Redacted text stays blocked"):
        pill(draw, (66, y), label, accent=(104, 205, 190))
        y += 52
    draw.rounded_rectangle((66, 526, 350, 652), radius=18, fill=(31, 42, 76), outline=(83, 105, 151), width=1)
    text(draw, (87, 546), "COPIED TEXT", 13, fill=(111, 214, 199), weight="bold")
    text(draw, (87, 579), "Kim Patel\nDashboard totals do not\nmatch export", 16, fill=(229, 235, 249), spacing=5)
    editor = ocr_editor_preview()
    paste_card(canvas, editor, (382, 89), (840, 610), radius=16, shadow=22)
    save(canvas, "04-select-text.jpg")


@lru_cache(maxsize=1)
def video_annotation_preview() -> Image.Image:
    video = Image.open(CAPTURES / "video-annotations" / "matte-none.png").convert("RGBA")
    layer = Image.new("RGBA", video.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(layer)
    red = (255, 77, 73, 255)
    draw.line((770, 383, 1050, 284), fill=red, width=8)
    draw.polygon([(1050, 284), (1019, 283), (1037, 313)], fill=red)
    draw.ellipse((1114, 445, 1170, 501), fill=(255, 193, 54), outline=(255, 255, 255), width=4)
    text(draw, (1142, 473), "1", 24, fill=(34, 40, 59), weight="bold", anchor="mm")
    video.alpha_composite(layer)
    return video


def slide_video():
    canvas = gradient(left=(21, 29, 59), right=(46, 33, 83))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (64, 123), "Annotate video.\nStay in the flow.", 40, weight="bold", spacing=2)
    text(draw, (66, 240), "The same nine tools as screenshots,\nnow time-ranged on video.", 20, fill=(193, 204, 229), spacing=7)
    y = 345
    for label in ("All 9 annotation tools", "Whole video or 3 seconds", "Full-res export with audio"):
        pill(draw, (66, y), label, accent=(117, 137, 235))
        y += 51
    video = video_annotation_preview()
    paste_card(canvas, video, (390, 83), (820, 615), radius=16, shadow=22)
    save(canvas, "05-video-editor.jpg")


def slide_capture_more():
    canvas = gradient(left=(20, 28, 56), right=(25, 54, 76))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (64, 121), "Capture what\ndoesn't fit.", 44, weight="bold", spacing=2)
    text(draw, (66, 240), "Keep long pages and important\nreferences useful after capture.", 21, fill=(193, 204, 229), spacing=7)
    y = 355
    for label in ("Scrolling capture", "Pin above other windows", "Ready-to-paste mattes"):
        pill(draw, (66, y), label, accent=(104, 205, 190))
        y += 53
    scroll = Image.new("RGB", (520, 1180), (249, 250, 253))
    sd = ImageDraw.Draw(scroll)
    sd.rectangle((0, 0, 520, 82), fill=(31, 39, 58))
    text(sd, (28, 29), "Northstar Help Center", 17, weight="bold")
    text(sd, (35, 125), "Incident response guide", 30, fill=(30, 40, 62), weight="bold")
    text(sd, (36, 177), "A calm, repeatable process for resolving\ncustomer-facing incidents.", 16, fill=(92, 106, 131), spacing=5)
    sections = [
        (255, "1. Confirm the impact", ["Identify affected customers", "Capture the current state", "Assign an incident owner"]),
        (487, "2. Communicate clearly", ["Post the first update", "Set the next update time", "Keep one source of truth"]),
        (719, "3. Resolve and verify", ["Test the fix end to end", "Monitor recovery metrics", "Close the communication loop"]),
        (951, "4. Learn and improve", ["Write a short timeline", "Record follow-up actions", "Share the lessons learned"]),
    ]
    for top, heading, rows in sections:
        sd.rounded_rectangle((30, top, 490, top + 196), radius=17, fill=(255, 255, 255), outline=(222, 228, 239), width=2)
        text(sd, (52, top + 25), heading, 20, fill=(35, 47, 72), weight="bold")
        for i, row in enumerate(rows):
            yy = top + 76 + i * 34
            sd.ellipse((52, yy + 2, 64, yy + 14), fill=(83, 188, 169))
            text(sd, (78, yy - 3), row, 15, fill=(76, 89, 111))
    scroll_h = round(scroll.height * 350 / scroll.width)
    scroll = scroll.resize((350, scroll_h), Image.Resampling.LANCZOS).crop((0, 0, 350, 575))
    paste_card(canvas, scroll, (826, 89), (350, 575), radius=18, shadow=20, contain=False)
    adaptive = Image.open(CAPTURES / "demo" / "matte-adaptive.png")
    paste_card(canvas, adaptive, (390, 182), (520, 330), radius=15, shadow=18)
    picker = Image.open(CAPTURES / "picker" / "raw.png")
    picker = fit(picker, (610, 150), contain=True)
    paste_card(canvas, picker, (365, 522), (610, 150), radius=13, shadow=16)
    save(canvas, "06-capture-more.jpg")


def slide_trust():
    canvas = gradient(left=(18, 26, 55), right=(45, 32, 86))
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas)
    text(draw, (635, 145), "Your screenshots stay yours.", 48, weight="bold", anchor="mm")
    text(draw, (635, 205), "A native Windows app built for speed, privacy and ownership.", 21, fill=(198, 207, 230), anchor="mm")
    cards = [
        ("LOCAL", "Capture, OCR and editing\nhappen on your PC."),
        ("SIGNED", "Verified installer and\ntrusted automatic updates."),
        ("ONE TIME", "$19 once. No account\nrequired for the trial."),
    ]
    for i, (head, body) in enumerate(cards):
        x = 83 + i * 393
        draw.rounded_rectangle((x, 294, x + 344, 514), radius=24, fill=(33, 42, 78), outline=(86, 102, 151), width=1)
        draw.ellipse((x + 136, 330, x + 208, 402), fill=(92, 113, 228), outline=(119, 218, 202), width=2)
        text(draw, (x + 172, 366), str(i + 1), 26, weight="bold", anchor="mm")
        text(draw, (x + 172, 436), head, 17, fill=(112, 215, 201), weight="bold", anchor="mm")
        text(draw, (x + 172, 477), body, 16, fill=(220, 227, 244), anchor="mm", spacing=4)
    draw.rounded_rectangle((455, 581, 815, 647), radius=33, fill=(92, 113, 228))
    text(draw, (635, 614), "Try everything free for 14 days", 20, weight="semibold", anchor="mm")
    save(canvas, "07-private-native.jpg")


def thumbnail():
    image = gradient((240, 240), left=(52, 60, 145), right=(114, 68, 164))
    icon = Image.open(ROOT.parents[1] / "assets" / "icon-256.png").convert("RGBA").resize((142, 142), Image.Resampling.LANCZOS)
    shadow = Image.new("RGBA", image.size, (0, 0, 0, 0))
    ImageDraw.Draw(shadow).ellipse((50, 66, 190, 206), fill=(0, 0, 0, 100))
    shadow = shadow.filter(ImageFilter.GaussianBlur(22))
    image.alpha_composite(shadow)
    image.alpha_composite(icon, (49, 39))
    draw = ImageDraw.Draw(image)
    text(draw, (120, 207), "MATTESHOT", 17, weight="bold", anchor="mm")
    image.convert("RGB").save(OUT / "thumbnail-240.png")


def ease(t: float) -> float:
    t = max(0.0, min(1.0, t))
    return t * t * (3 - 2 * t)


def cursor(draw: ImageDraw.ImageDraw, x: float, y: float, click=0.0):
    if click > 0:
        r = 13 + click * 25
        a = int(180 * (1 - click))
        draw.ellipse((x - r, y - r, x + r, y + r), outline=(104, 220, 201, a), width=4)
    points = [(x, y), (x + 3, y + 31), (x + 12, y + 23), (x + 20, y + 40), (x + 28, y + 36), (x + 20, y + 19), (x + 32, y + 17)]
    draw.polygon(points, fill=(255, 255, 255), outline=(14, 18, 35))


def demo_frame(time_s: float) -> Image.Image:
    canvas = cached_gradient(left=(18, 25, 57), right=(42, 37, 96)).copy()
    draw = ImageDraw.Draw(canvas)
    add_brand(canvas, (52, 35))
    raw = cached_image(str(CAPTURES / "demo" / "raw.png"))
    picker_img = cached_image(str(CAPTURES / "picker" / "raw.png"))
    aurora = cached_image(str(CAPTURES / "demo" / "matte-aurora.png"))
    ocr_editor = ocr_editor_preview()
    video_editor = video_annotation_preview()

    if time_s < 2.4:
        text(draw, (635, 116), "Press PrtScn", 47, weight="bold", anchor="mm")
        text(draw, (635, 169), "Click a window or drag a region.", 20, fill=(198, 208, 232), anchor="mm")
        scale = 0.94 + ease(time_s / 2.4) * 0.035
        box = (round(920 * scale), round(548 * scale))
        pos = ((W - box[0]) // 2, 200 + (548 - box[1]) // 2)
        x, y, iw, ih = paste_card(canvas, raw, pos, box, radius=17, shadow=20)
        cursor(draw, x + iw * .82, y + ih * .18)
    elif time_s < 4.8:
        local = time_s - 2.4
        text(draw, (635, 116), "Pick a finished look", 45, weight="bold", anchor="mm")
        text(draw, (635, 169), "The first result is already copied.", 20, fill=(198, 208, 232), anchor="mm")
        p = fit(picker_img, (1170, 250), contain=True)
        paste_card(canvas, p, (50, 250), (1170, 250), radius=16, shadow=18)
        start, end = (335, 390), (620, 390)
        m = ease(local / 1.6)
        cx = start[0] + (end[0] - start[0]) * m
        cy = start[1] + (end[1] - start[1]) * m
        click = max(0.0, min(1.0, (local - 1.65) / .65)) if local > 1.65 else 0.0
        cursor(draw, cx, cy, click)
        draw.rounded_rectangle((455, 586, 815, 641), radius=27, fill=(67, 178, 151))
        text(draw, (635, 613), "Six styles. One click.", 20, weight="semibold", anchor="mm")
    elif time_s < 7.3:
        text(draw, (635, 92), "Select text from the screenshot", 43, weight="bold", anchor="mm")
        text(draw, (635, 143), "Offline OCR. Drag, copy and keep moving.", 20, fill=(198, 208, 232), anchor="mm")
        paste_card(canvas, ocr_editor, (105, 180), (1060, 500), radius=16, shadow=22)
        draw.rounded_rectangle((879, 631, 1120, 702), radius=18, fill=(31, 42, 76), outline=(104, 205, 190), width=2)
        text(draw, (899, 644), "COPIED", 12, fill=(111, 214, 199), weight="bold")
        text(draw, (899, 668), "Dashboard totals do not match export", 14, fill=(232, 237, 249))
    elif time_s < 10.3:
        text(draw, (635, 89), "Annotate video, too", 44, weight="bold", anchor="mm")
        text(draw, (635, 140), "Nine tools, precise timing and audio-preserving export.", 20, fill=(198, 208, 232), anchor="mm")
        paste_card(canvas, video_editor, (100, 178), (1070, 530), radius=16, shadow=22)
    else:
        local = time_s - 10.3
        text(draw, (635, 92), "Paste anywhere.", 45, weight="bold", anchor="mm")
        text(draw, (635, 143), "Email, chat, docs or your next launch.", 20, fill=(198, 208, 232), anchor="mm")
        draw.rounded_rectangle((145, 187, 1125, 700), radius=22, fill=(244, 246, 250), outline=(255, 255, 255, 40), width=1)
        draw.rectangle((145, 187, 1125, 247), fill=(35, 39, 50))
        text(draw, (178, 217), "New message", 18, weight="semibold", anchor="lm")
        text(draw, (181, 277), "To:  Product team", 16, fill=(60, 68, 84))
        draw.line((180, 305, 1090, 305), fill=(210, 216, 227), width=1)
        text(draw, (181, 335), "The support dashboard is ready for review.", 17, fill=(47, 54, 70))
        pasted = fit(aurora, (660, 305), contain=True)
        y = 372 + round(15 * (1 - ease(local / 1.2)))
        alpha = int(255 * ease(local / .8))
        pasted.putalpha(alpha)
        canvas.alpha_composite(pasted, ((W - pasted.width) // 2, y))
        draw.rounded_rectangle((902, 646, 1080, 684), radius=19, fill=(92, 113, 228))
        text(draw, (991, 665), "Send", 16, weight="semibold", anchor="mm")
    return canvas.convert("RGB")


def demo_video():
    fps = 30
    duration = 13.8
    mp4 = OUT / "matteshot-product-hunt-demo.mp4"
    command = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{W}x{H}",
        "-r", str(fps), "-i", "-", "-an", "-c:v", "libx264", "-preset", "medium",
        "-crf", "18", "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(mp4),
    ]
    proc = subprocess.Popen(command, stdin=subprocess.PIPE)
    if proc.stdin is None:
        raise RuntimeError("ffmpeg did not expose a frame input stream")
    try:
        for i in range(round(duration * fps)):
            proc.stdin.write(demo_frame(i / fps).tobytes())
    except BrokenPipeError as error:
        try:
            proc.stdin.close()
        except BrokenPipeError:
            pass
        code = proc.wait()
        raise RuntimeError(f"ffmpeg stopped while encoding frames (exit code {code})") from error
    proc.stdin.close()
    code = proc.wait()
    if code != 0:
        raise RuntimeError(f"ffmpeg failed to encode the demo (exit code {code})")

    palette = OUT / "demo-palette.png"
    gif = OUT / "matteshot-product-hunt-demo.gif"
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(mp4),
        "-vf", "fps=10,scale=760:-1:flags=lanczos,palettegen=max_colors=128", str(palette)
    ], check=True)
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-i", str(mp4), "-i", str(palette), "-lavfi",
        "fps=10,scale=760:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=4", str(gif)
    ], check=True)
    palette.unlink(missing_ok=True)
    if gif.stat().st_size > 3_000_000:
        raise RuntimeError(f"Product Hunt GIF is too large: {gif.stat().st_size:,} bytes")


def main():
    slide_hero()
    slide_styles()
    slide_editor()
    slide_ocr()
    slide_video()
    slide_capture_more()
    slide_trust()
    thumbnail()
    demo_video()
    for file in sorted(OUT.iterdir()):
        print(f"{file.name}: {file.stat().st_size:,} bytes")


if __name__ == "__main__":
    main()
