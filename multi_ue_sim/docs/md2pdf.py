#!/usr/bin/env python3

#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

#
# Reusable Markdown -> PDF converter (WeasyPrint), styled to match the
# OCUDU_Slicing_*.pdf design docs. Usage:
#     python3 md2pdf.py INPUT.md [OUTPUT.pdf]
# If OUTPUT is omitted it is INPUT with the .pdf extension.

import sys
import os
import re
import markdown
from weasyprint import HTML

CSS = """
@page {
    size: A4;
    margin: 16mm 15mm 18mm 15mm;
    @bottom-center {
        content: "OCUDU \\2014 Multi-UE Dashboard 2.0 Design";
        font-family: 'DejaVu Sans', 'Helvetica Neue', Arial, sans-serif;
        font-size: 7.5pt; color: #9aa0a6;
    }
    @bottom-right {
        content: "Page " counter(page) " / " counter(pages);
        font-family: 'DejaVu Sans', Arial, sans-serif;
        font-size: 7.5pt; color: #9aa0a6;
    }
}
* { box-sizing: border-box; }
body {
    font-family: 'DejaVu Sans', 'Helvetica Neue', Arial, sans-serif;
    font-size: 9.6pt; line-height: 1.5; color: #1f2329;
    -weasy-hyphens: none;
}
h1, h2, h3, h4 {
    font-family: 'DejaVu Sans', 'Helvetica Neue', Arial, sans-serif;
    color: #0b3d66; line-height: 1.25; font-weight: 700;
}
h1 {
    font-size: 20pt; margin: 0 0 6pt 0; padding-bottom: 6pt;
    border-bottom: 3px solid #0b6fb8;
}
h2 {
    font-size: 14pt; margin: 18pt 0 6pt 0; padding-bottom: 3pt;
    border-bottom: 1px solid #cdd6df; color: #0b6fb8;
    page-break-after: avoid;
}
h3 {
    font-size: 11.5pt; margin: 13pt 0 4pt 0; color: #16548a;
    page-break-after: avoid;
}
h4 { font-size: 10pt; margin: 10pt 0 3pt 0; color: #355; page-break-after: avoid; }
p { margin: 5pt 0; }
a { color: #0b6fb8; text-decoration: none; }
strong { color: #0b2a45; }
hr { border: none; border-top: 1px solid #d6dde4; margin: 14pt 0; }

ul, ol { margin: 5pt 0 5pt 0; padding-left: 18pt; }
li { margin: 2.5pt 0; }

/* inline code */
code {
    font-family: 'DejaVu Sans Mono', 'Consolas', monospace;
    font-size: 8.4pt; background: #eef2f6; color: #0b3d66;
    padding: 0.5pt 3pt; border-radius: 3px;
}
/* code blocks */
pre {
    background: #1f2937; color: #e6edf3;
    border-radius: 6px; padding: 8pt 10pt; margin: 7pt 0;
    font-size: 7.9pt; line-height: 1.42; overflow-wrap: break-word;
    white-space: pre-wrap; page-break-inside: avoid;
    border: 1px solid #11161f;
}
pre code { background: transparent; color: inherit; padding: 0; font-size: 7.9pt; }
.codehilite { background: #1f2937; border-radius: 6px; margin: 7pt 0; page-break-inside: avoid; }
.codehilite pre { margin: 0; border: none; }
/* pygments token colors on dark bg */
.codehilite .k, .codehilite .kd, .codehilite .kn { color: #c792ea; }
.codehilite .s, .codehilite .s1, .codehilite .s2, .codehilite .se { color: #c3e88d; }
.codehilite .c, .codehilite .c1, .codehilite .cm { color: #8b98a8; font-style: italic; }
.codehilite .nf, .codehilite .nc { color: #82aaff; }
.codehilite .mi, .codehilite .mf, .codehilite .m { color: #f78c6c; }
.codehilite .o, .codehilite .p { color: #89ddff; }
.codehilite .nb, .codehilite .bp { color: #ffcb6b; }

/* tables */
table {
    border-collapse: collapse; width: 100%; margin: 8pt 0;
    font-size: 8.4pt; page-break-inside: avoid;
}
th, td {
    border: 1px solid #cdd6df; padding: 4pt 6pt;
    text-align: left; vertical-align: top;
}
th {
    background: #0b6fb8; color: #fff; font-weight: 700;
    border-color: #0b5a96;
}
tr:nth-child(even) td { background: #f3f6f9; }
td code { font-size: 7.8pt; }

/* blockquote = the "honesty" callouts */
blockquote {
    margin: 8pt 0; padding: 6pt 11pt;
    background: #fff8e6; border-left: 4px solid #e0a800;
    color: #4a3c10; font-size: 9pt;
}
blockquote p { margin: 2pt 0; }
"""

def convert(src, dst):
    with open(src, "r", encoding="utf-8") as f:
        text = f.read()

    md = markdown.Markdown(extensions=[
        "extra",          # tables, fenced_code, etc.
        "codehilite",
        "sane_lists",
        "toc",
        "admonition",
    ], extension_configs={
        "codehilite": {"guess_lang": False, "noclasses": False},
    })
    body = md.convert(text)

    html = f"""<!DOCTYPE html><html><head><meta charset="utf-8">
<style>{CSS}</style></head><body>{body}</body></html>"""

    HTML(string=html, base_url=os.path.dirname(os.path.abspath(src))).write_pdf(dst)
    print(f"wrote {dst} ({os.path.getsize(dst):,} bytes)")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: md2pdf.py INPUT.md [OUTPUT.pdf]")
    src = sys.argv[1]
    dst = sys.argv[2] if len(sys.argv) > 2 else re.sub(r"\.md$", "", src) + ".pdf"
    convert(src, dst)