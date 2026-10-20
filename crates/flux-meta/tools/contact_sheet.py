"""Contact sheet of images with ffmpeg: contact_sheet.py OUT.jpg COLS TILE_W TILE_H files..."""
import subprocess, sys
out, cols, tw, th, files = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5:]
rows = (len(files) + cols - 1) // cols
args, fc = [], ""
for i, f in enumerate(files):
    args += ["-i", f]
    fc += f"[{i}:v]scale={tw}:{th}:force_original_aspect_ratio=decrease,pad={tw}:{th}:(ow-iw)/2:(oh-ih)/2[s{i}];"
n = len(files)
for i in range(n, rows * cols):  # pad with black tiles
    fc += f"color=black:s={tw}x{th}:d=1[s{i}];"
for r in range(rows):
    fc += "".join(f"[s{r*cols+c}]" for c in range(cols)) + f"hstack={cols}[r{r}];"
fc += "".join(f"[r{r}]" for r in range(rows)) + (f"vstack={rows}" if rows > 1 else "null")
subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args, "-filter_complex", fc, "-frames:v", "1", out], check=True)
print(out)
