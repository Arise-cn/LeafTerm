#!/usr/bin/env python3
"""Render LeafTerm's illustrative README animation using vector-style drawing.

Requires Pillow. All project names, code and CLI output are fictional; this is
not a screen recording. Run from any directory: python3 scripts/render-readme-demo.py
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'docs' / 'images'
W, H = 1200, 720
BG = '#080F16'
PANEL = '#0E1820'
SURFACE = '#14232B'
LINE = '#253A43'
WHITE = '#E5F0F2'
MUTED = '#91A9B4'
MINT = '#6DDEAA'
CYAN = '#7DCDE8'
GOLD = '#E7BE6F'
RED = '#EA888E'


def font(size, mono=False, bold=False):
    # macOS fonts first, with common Linux equivalents for regeneration.
    candidates = (
        ['/System/Library/Fonts/Menlo.ttc', '/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf']
        if mono else
        ['/System/Library/Fonts/Supplemental/Arial Bold.ttf' if bold else '/System/Library/Fonts/Supplemental/Arial.ttf',
         '/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf' if bold else '/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf']
    )
    for candidate in candidates:
        if Path(candidate).exists():
            return ImageFont.truetype(candidate, size)
    raise RuntimeError('Install Arial/Menlo or DejaVu Sans/DejaVu Sans Mono to render the demo.')


F = {name: font(*args) for name, args in {
    'hero': (38, False, True), 'body': (19,), 'brand': (18, False, True),
    'small': (13,), 'label': (13, False, True), 'mono': (18, True),
    'code': (17, True), 'tiny': (13, True), 'medium': (16, False, True),
}.items()}


def text(d, xy, value, fill=WHITE, face='body'):
    d.text(xy, value, font=F[face], fill=fill)


def pill(d, box, label, active=False):
    d.rounded_rectangle(box, radius=7, fill='#19372D' if active else SURFACE,
                        outline=MINT if active else LINE, width=1)
    width = d.textlength(label, font=F['label'])
    text(d, ((box[0]+box[2]-width)/2, box[1]+8), label, MINT if active else MUTED, 'label')


def sidebar(d, phase, p):
    d.rectangle((45, 232, 304, 622), fill='#111E25')
    d.line((304, 232, 304, 622), fill=LINE)
    text(d, (66, 253), 'nebula', WHITE, 'brand')
    text(d, (66, 282), 'git  feat/agent-workflow', MINT, 'tiny')
    text(d, (66, 324), 'FILES', MINT, 'label')
    text(d, (159, 324), 'CHANGES  3', MUTED, 'label')
    d.line((66, 349, 282, 349), fill=LINE)
    lines = [('v  config', ''), ('   app.toml', 'M'), ('v  src', ''),
             ('   main.rs', ''), ('   settings.rs', 'M'), ('>  tests', ''), ('   Cargo.toml', '')]
    selected = 1 if phase in (1, 2, 3) else 4
    for i, (name, status) in enumerate(lines):
        y = 365+i*31
        if i == selected:
            d.rounded_rectangle((56, y-3, 292, y+25), radius=4, fill='#1B3035')
            d.rectangle((56, y-3, 58, y+25), fill=MINT)
        text(d, (68, y), name, WHITE if i == selected else MUTED, 'tiny')
        if status:
            text(d, (266, y), status, GOLD, 'tiny')
    text(d, (66, 598), 'PROJECT CONTEXT, ON DEMAND', '#637E8A', 'small')


def code_line(d, y, number, segments, x=349):
    text(d, (x-22, y+1), str(number), '#536E79', 'tiny')
    cursor = x+13
    for value, color in segments:
        text(d, (cursor, y), value, color, 'code')
        cursor += d.textlength(value, font=F['code'])


TITLES = [
    ('AI coding. One focused workspace.', 'Run your CLI. Keep your project within reach.'),
    ('See exactly what changed.', 'Check the branch, follow the files, review the diff.'),
    ('One switch. No extra IDE.', 'Open a config file, make the edit, get back to your agent.'),
    ('Resolve the conflict. Stay in flow.', 'Choose either side or keep both. Undo when you need to.'),
    ('Give the terminal its space.', 'Hide the sidebar with a shortcut. Bring it back when you need it.'),
]


def frame(phase, step):
    p = step / 29
    im = Image.new('RGB', (W, H), BG)
    d = ImageDraw.Draw(im)
    # Restrained circuit grid and corner markers, never real project content.
    for x in range(0, W, 40):
        d.line((x, 0, x, H), fill='#0C1922')
    for y in range(0, H, 40):
        d.line((0, y, W, y), fill='#0C1922')
    for x, y in ((26, 170), (1174, 170), (26, 642), (1174, 642)):
        d.line((x-7, y, x+7, y), fill='#3B6668')
        d.line((x, y-7, x, y+7), fill='#3B6668')
    # Brand symbol is an original vector mark, not an edited raster image.
    d.rounded_rectangle((44, 32, 77, 65), radius=9, fill='#14342E', outline='#37745D')
    d.line((54, 42, 61, 48, 54, 54), fill=MINT, width=3)
    d.line((64, 54, 69, 54), fill=MINT, width=2)
    text(d, (89, 39), 'LEAFTERM', MINT, 'brand')
    text(d, (873, 42), f'WORKFLOW DEMO     0{phase+1} / 05', MUTED, 'tiny')
    text(d, (44, 91), TITLES[phase][0], WHITE, 'hero')
    text(d, (46, 145), TITLES[phase][1], MUTED, 'body')
    # Work surface.
    d.rounded_rectangle((44, 194, 1156, 624), radius=14, fill=PANEL, outline='#36545D', width=1)
    d.rounded_rectangle((44, 194, 1156, 233), radius=14, fill='#17262E')
    d.rectangle((45, 219, 1155, 233), fill='#17262E')
    for x, color in ((65, '#DC7E86'), (85, GOLD), (105, MINT)):
        d.ellipse((x, 208, x+8, 216), fill=color)
    text(d, (437, 204), 'nebula  /  feat/agent-workflow', MUTED, 'tiny')
    text(d, (1054, 204), 'DEMO', '#63808B', 'tiny')
    d.line((45, 233, 1155, 233), fill=LINE)
    if phase < 4:
        sidebar(d, phase, p)
    if phase == 0:
        text(d, (338, 258), '> codex', MINT, 'mono')
        prompt = 'Add a preview toggle to this workspace.'
        count = min(len(prompt), int(p*len(prompt)*3.2))
        text(d, (338, 300), prompt[:count], WHITE, 'mono')
        if step < 10:
            x = 338+d.textlength(prompt[:count], font=F['mono'])
            d.rectangle((x+3, 300, x+5, 320), fill=MINT)
        for index, (label, color) in enumerate([
            ('Read     config/app.toml', MUTED),
            ('Update   src/settings.rs', CYAN),
            ('Review   changes in the sidebar', MINT),
        ]):
            if step >= 9+index*5:
                y = 364+index*43
                d.ellipse((340, y+7, 346, y+13), fill=color)
                text(d, (361, y), label, color, 'mono')
        text(d, (338, 571), 'Your CLI. Your shell. Your workflow.', '#7F9AA6', 'code')
    elif phase == 1:
        text(d, (338, 255), 'config/app.toml', WHITE, 'medium')
        pill(d, (970, 249, 1129, 282), '+2  -1', True)
        rows = [('[features]', CYAN), (' preview = false', RED), (' preview = true', MINT),
                (' file_search = true', WHITE), (' syntax_colors = true', MINT)]
        for i, (value, color) in enumerate(rows):
            y = 318+i*43
            if i in (1, 2, 4):
                d.rounded_rectangle((332, y-4, 1131, y+32), radius=4, fill='#32232A' if i == 1 else '#19352C')
            prefix = '-' if i == 1 else '+' if i in (2,4) else ' '
            text(d, (343, y), prefix, color, 'mono')
            text(d, (372, y), value, color, 'mono')
        text(d, (338, 574), 'Review what changed before the next step.', MUTED, 'code')
        # A scanning indicator suggests inspection without flashing.
        x = int(338+min(p, 0.8)/0.8*788)
        d.line((338, 298, x, 298), fill=MINT, width=2)
    elif phase == 2:
        text(d, (338, 255), 'config/app.toml', WHITE, 'medium')
        pill(d, (1012, 249, 1129, 282), 'CMD + S', step >= 17)
        edited = step >= 9
        rows = [
            [('[workspace]', CYAN)], [('name', MINT), (' = ', WHITE), ('"nebula"', MINT)], [],
            [('[features]', CYAN)], [('preview', MINT), (' = ', WHITE), ('true' if edited else 'false', GOLD)],
            [('file_search', MINT), (' = ', WHITE), ('true', GOLD)],
            [('syntax_colors', MINT), (' = ', WHITE), ('true', GOLD)],
        ]
        d.rounded_rectangle((327, 452, 1130, 487), radius=4, fill='#1B3035')
        for i, segs in enumerate(rows):
            code_line(d, 316+i*35, i+1, segs)
        if 9 <= step < 17:
            text(d, (1031, 575), 'Modified', GOLD, 'tiny')
        elif step >= 17:
            text(d, (1060, 575), 'Saved', MINT, 'tiny')
        text(d, (339, 575), 'TOML  /  syntax highlighting', MUTED, 'tiny')
    elif phase == 3:
        text(d, (338, 255), 'config/app.toml  /  merge conflict', WHITE, 'medium')
        left, right = (332, 301, 722, 407), (738, 301, 1130, 407)
        for box, label in ((left, 'CURRENT'), (right, 'INCOMING')):
            d.rounded_rectangle(box, radius=7, fill='#12232B', outline=LINE)
            text(d, (box[0]+16, box[1]+13), label, CYAN, 'label')
        text(d, (349, 355), 'preview = true', WHITE, 'code')
        text(d, (755, 355), 'file_search = true', WHITE, 'code')
        for box, label in [((332,421,559,456),'ACCEPT CURRENT'),((573,421,800,456),'ACCEPT INCOMING'),((814,421,1130,456),'ACCEPT BOTH')]:
            pill(d, box, label, step >= 9 and label == 'ACCEPT BOTH')
        text(d, (338, 483), 'RESULT', MUTED, 'label')
        if step >= 9:
            text(d, (339, 514), 'preview = true', MINT, 'code')
            text(d, (339, 546), 'file_search = true', MINT, 'code')
        else:
            text(d, (339, 518), 'Choose how to resolve this section.', MUTED, 'code')
        text(d, (949, 582), 'CMD + Z  Undo', MUTED, 'tiny')
    else:
        # Slide the sidebar off the work surface during the first 0.6 seconds.
        amount = max(0, 1-step/6)
        if amount > 0:
            layer = Image.new('RGB', (W, H), PANEL)
            ld = ImageDraw.Draw(layer); sidebar(ld, phase, p)
            strip = layer.crop((45, 234, 305, 622))
            visible = int(260*amount)
            im.paste(strip.crop((260-visible, 0, 260, 388)), (45, 234))
        offset = int(260*amount)
        x = 82+offset
        text(d, (x, 266), '> codex', MINT, 'mono')
        text(d, (x, 310), 'Continue building. Everything else can wait.', WHITE, 'mono')
        d.line((x, 353, 1117, 353), fill=LINE)
        text(d, (x, 389), 'More room for the conversation.', CYAN, 'body')
        text(d, (x, 427), 'Project context is one shortcut away.', MUTED, 'body')
        pill(d, (x, 493, x+194, 530), 'CMD + B  /  SIDEBAR', True)
        if step % 10 < 6:
            d.rectangle((x, 568, x+9, 588), fill=MINT)
    # Timeline and honest provenance remain visible in every frame.
    labels = ['01  RUN', '02  REVIEW', '03  EDIT', '04  RESOLVE', '05  FOCUS']
    for i, label in enumerate(labels):
        x = 44+i*226
        text(d, (x, 650), label, MINT if i == phase else '#708995', 'label')
        d.rounded_rectangle((x, 676, x+207, 679), radius=1, fill='#20313A')
        if i == phase:
            d.rounded_rectangle((x, 676, x+max(2, int(207*p)), 679), radius=1, fill=MINT)
        elif i < phase:
            d.rectangle((x, 676, x+207, 679), fill='#386A5A')
    text(d, (44, 696), 'ILLUSTRATIVE UI  /  FICTIONAL PROJECT', '#6B8792', 'small')
    text(d, (805, 696), 'v0.1.0  /  macOS preview  /  Windows in development', '#6B8792', 'small')
    return im


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    # One palette across all frames prevents distracting color flicker.
    keyframes = [frame(i, 20) for i in range(5)]
    palette_source = Image.new('RGB', (W, H*5))
    for i, im in enumerate(keyframes):
        palette_source.paste(im, (0, i*H))
    palette = palette_source.quantize(colors=192, method=Image.Quantize.MEDIANCUT)
    frames = [frame(i, j).quantize(palette=palette, dither=Image.Dither.NONE)
              for i in range(5) for j in range(30)]
    dest = OUT / 'workflow.gif'
    frames[0].save(dest, save_all=True, append_images=frames[1:], duration=100,
                   loop=0, optimize=True, disposal=1)
    keyframes[0].save(OUT / 'workflow-poster.png', optimize=True)
    print(f'{dest}: {len(frames)} frames, 15 seconds, {dest.stat().st_size / 1024:.0f} KiB')


if __name__ == '__main__':
    main()
