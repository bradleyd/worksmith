#!/usr/bin/env python3
"""Generate chapter.docx — a minimal book chapter using No Starch-style Word
paragraph styles (HeadA / Body / Code). Regenerate with:  python3 make_chapter.py
"""
import zipfile
from pathlib import Path

OUT = Path(__file__).resolve().parent / "chapter.docx"

CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>
</Types>"""

RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"""

DOC_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"""

STYLES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Body"/></w:style>
  <w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/></w:style>
  <w:style w:type="paragraph" w:styleId="HeadA"><w:name w:val="HeadA"/></w:style>
</w:styles>"""


def para(style: str, text: str) -> str:
    return (f'<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>'
            f'<w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>')


DOCUMENT = ("""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
""" + "\n".join([
    para("HeadA", "Introduction"),
    para("Body", "This chapter introduces the power tool and how it fits your workflow."),
    para("Code", "let tool = PowerTool::new();"),
    para("HeadA", "Details"),
    para("Body", "The details section explains the internals and the trade-offs."),
]) + """
    <w:sectPr/>
  </w:body>
</w:document>""")


def main() -> None:
    with zipfile.ZipFile(OUT, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml", CONTENT_TYPES)
        z.writestr("_rels/.rels", RELS)
        z.writestr("word/_rels/document.xml.rels", DOC_RELS)
        z.writestr("word/styles.xml", STYLES)
        z.writestr("word/document.xml", DOCUMENT)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
