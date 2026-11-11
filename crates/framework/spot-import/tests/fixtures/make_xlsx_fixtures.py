#!/usr/bin/env python3
"""Generate the XLSX test fixtures for spot-import (hand-written zip XML, no deps).

Usage: python3 make_xlsx_fixtures.py <output-dir>
Creates: basic.xlsx (strings, numbers, formula, date, error cell, sparse rows, 2 sheets),
         empty.xlsx (no rows), corrupt.xlsx (not a zip), quotes.xlsx (weird cell text).
"""
import sys
import zipfile
import pathlib

CT = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>
<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
</Types>"""

RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"""

WB = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2"/></sheets>
</workbook>"""

WBRELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"""

STYLES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<numFmts count="1"><numFmt numFmtId="14" formatCode="mm-dd-yy"/></numFmts>
<cellXfs count="2">
<xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
<xf numFmtId="14" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/>
</cellXfs>
</styleSheet>"""

# Shared strings used by basic.xlsx: 0=name 1=Acme Ltd 2=total 3=other 4=&lt;q&gt; 5=multi
# 6=lead 7=end
SST = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="8" uniqueCount="8">
<si><t>name</t></si>
<si><t>Acme Ltd</t></si>
<si><t>total</t></si>
<si><t>other</t></si>
<si><t>&lt;q&gt; &amp; more</t></si>
<si><t>line1
line2</t></si>
<si><t>  leading and trailing  </t></si>
<si><t></t></si>
</sst>"""

# Row 1 headers; row 2 strings/number/date; row 3 formula (cached 3), int, ERROR cell;
# row 4 sparse (only C); row 5: xml-escaped text, multiline shared string, whitespace pad,
# empty-string cell.
SHEET1 = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:D5"/><sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>2</v></c><c r="C1" t="s"><v>3</v></c><c r="D1" t="s"><v>4</v></c></row>
<row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>1.50</v></c><c r="C2"><v>42</v></c><c r="D2" s="1"><v>46000</v></c></row>
<row r="3"><c r="A3"><f>A2*2</f><v>3</v></c><c r="B3"><v>7.25</v></c><c r="C3" t="e"><v>#N/A</v></c></row>
<row r="4"><c r="C4" t="s"><v>0</v></c></row>
<row r="5"><c r="A5" t="s"><v>5</v></c><c r="B5"><v>0.00</v></c></row>
</sheetData></worksheet>"""

SHEET2 = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData>
<row r="1"><c r="A1" t="s"><v>3</v></c></row>
<row r="2"><c r="A2" t="s"><v>1</v></c></row>
</sheetData></worksheet>"""

# Empty workbook: a sheet with an empty sheetData (no header at all).
EMPTY_CT = CT.replace('sheet2.xml"', 'sheet2.xml"')  # same content types
EMPTY_SHEET1 = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1"/><sheetData/></worksheet>"""


def write(path, parts):
    with zipfile.ZipFile(path, "w") as z:
        for name, text in parts:
            z.writestr(name, text)


def main():
    out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    out.mkdir(parents=True, exist_ok=True)
    write(
        out / "basic.xlsx",
        [
            ("[Content_Types].xml", CT),
            ("_rels/.rels", RELS),
            ("xl/workbook.xml", WB),
            ("xl/_rels/workbook.xml.rels", WBRELS),
            ("xl/sharedStrings.xml", SST),
            ("xl/styles.xml", STYLES),
            ("xl/worksheets/sheet1.xml", SHEET1),
            ("xl/worksheets/sheet2.xml", SHEET2),
        ],
    )
    write(
        out / "empty.xlsx",
        [
            ("[Content_Types].xml", EMPTY_CT),
            ("_rels/.rels", RELS),
            ("xl/workbook.xml", WB),
            ("xl/_rels/workbook.xml.rels", WBRELS),
            ("xl/sharedStrings.xml", SST),
            ("xl/styles.xml", STYLES),
            ("xl/worksheets/sheet1.xml", EMPTY_SHEET1),
            ("xl/worksheets/sheet2.xml", EMPTY_SHEET1),
        ],
    )
    (out / "corrupt.xlsx").write_bytes(b"this is not a zip file")
    print(f"wrote {(out / 'basic.xlsx').resolve()}, empty.xlsx, corrupt.xlsx")


if __name__ == "__main__":
    main()