#!/usr/bin/env python3
"""Fix stray/broken `..Default::default()` insertions in office_import.rs."""
import sys

path = "crates/core/src/convert/office_import.rs"
with open(path, "r", encoding="utf-8") as f:
    src = f.read()

original = src


def rep(needle, repl, label):
    global src
    count = src.count(needle)
    if count != 1:
        sys.exit(f"[{label}] expected exactly 1 occurrence, found {count}")
    src = src.replace(needle, repl)
    print(f"[{label}] fixed")


# Site 1: para_style_model (all new fields already wired — drop stray tokens)
rep(
    "        keep_with_next: para.keep_with_next,\n"
    "        keep_together: para.keep_together,\n"
    "    \n"
    "    ..Default::default()\n"
    "}\n"
    "\n"
    "..Default::default()\n"
    "}\n",
    "        keep_with_next: para.keep_with_next,\n"
    "        keep_together: para.keep_together,\n"
    "    }\n"
    "}\n",
    "para_style_model",
)

# Site 2: PptxParaPr::to_style
rep(
    "            line_height: self.line_height.unwrap_or_default(),\n"
    "        \n"
    "        ..Default::default()\n"
    "}\n"
    "    \n"
    "    ..Default::default()\n"
    "}\n"
    "}\n",
    "            line_height: self.line_height.unwrap_or_default(),\n"
    "            ..Default::default()\n"
    "        }\n"
    "    }\n"
    "}\n",
    "PptxParaPr::to_style",
)

# Site 3: odf_paragraph_style
rep(
    "        .to_paragraph_style(list_level)\n"
    "\n"
    "..Default::default()\n"
    "}\n",
    "        .to_paragraph_style(list_level)\n"
    "}\n",
    "odf_paragraph_style",
)

# Site 4: docx_style_to_paragraph
rep(
    "            None => MLineHeight::Normal,\n"
    "        },\n"
    "    \n"
    "    ..Default::default()\n"
    "}\n"
    "\n"
    "..Default::default()\n"
    "}\n",
    "            None => MLineHeight::Normal,\n"
    "        },\n"
    "        ..Default::default()\n"
    "    }\n"
    "}\n",
    "docx_style_to_paragraph",
)

# Site 5: OdfParaProps::to_paragraph_style
rep(
    "            line_height: self.line_height.unwrap_or(MLineHeight::Normal),\n"
    "        \n"
    "        ..Default::default()\n"
    "}\n"
    "    \n"
    "    ..Default::default()\n"
    "}\n"
    "}\n",
    "            line_height: self.line_height.unwrap_or(MLineHeight::Normal),\n"
    "            ..Default::default()\n"
    "        }\n"
    "    }\n"
    "}\n",
    "OdfParaProps::to_paragraph_style",
)

# Site 6: test helper
rep(
    '        find(&doc.sections[0].pages[0].blocks).expect("a paragraph/heading block")\n'
    "    \n"
    "    ..Default::default()\n"
    "}\n",
    '        find(&doc.sections[0].pages[0].blocks).expect("a paragraph/heading block")\n'
    "    }\n",
    "test helper",
)

if src == original:
    sys.exit("No changes made — patterns not found!")
with open(path, "w", encoding="utf-8") as f:
    f.write(src)
print("All sites fixed; file written.")
