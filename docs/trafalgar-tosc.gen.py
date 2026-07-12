#!/usr/bin/env python3
"""Generate a TouchOSC (.tosc) layout for Trafalgar's OSC-in contract.

Format (reverse-engineered via tosclib examples): a zlib-compressed UTF-8 XML
tree rooted at <lexml version='3'> with one root <node type='GROUP'>. Controls are
child <node>s with <properties>, <values>, <messages>. .tosc = zlib.compress(xml).
"""
import uuid, zlib, os

def uid():
    return str(uuid.uuid1())

# ---- property / value / message builders -----------------------------------
def p_b(k, v):   return f"<property type='b'><key><![CDATA[{k}]]></key><value>{v}</value></property>"
def p_i(k, v):   return f"<property type='i'><key><![CDATA[{k}]]></key><value>{v}</value></property>"
def p_f(k, v):   return f"<property type='f'><key><![CDATA[{k}]]></key><value>{v}</value></property>"
def p_s(k, v):   return f"<property type='s'><key><![CDATA[{k}]]></key><value><![CDATA[{v}]]></value></property>"
def p_c(k, c):   return (f"<property type='c'><key><![CDATA[{k}]]></key><value>"
                         f"<r>{c[0]}</r><g>{c[1]}</g><b>{c[2]}</b><a>{c[3]}</a></value></property>")
def p_r(k, x, y, w, h):
    return (f"<property type='r'><key><![CDATA[{k}]]></key><value>"
            f"<x>{x}</x><y>{y}</y><w>{w}</w><h>{h}</h></value></property>")

def val(key, default):
    return (f"<value><key><![CDATA[{key}]]></key><locked>0</locked>"
            f"<lockedDefaultCurrent>0</lockedDefaultCurrent>"
            f"<default><![CDATA[{default}]]></default><defaultPull>0</defaultPull></value>")

def osc(address, args, triggers):
    """address: full OSC string. args: list of value-keys sent as floats.
       triggers: list of value-keys whose change fires the send."""
    trg = "".join(f"<trigger><var><![CDATA[{t}]]></var><condition>ANY</condition></trigger>" for t in triggers)
    path = (f"<path><partial><type>CONSTANT</type><conversion>STRING</conversion>"
            f"<value><![CDATA[{address}]]></value><scaleMin>0</scaleMin><scaleMax>1</scaleMax></partial></path>")
    argp = "".join(
        "<partial><type>VALUE</type><conversion>FLOAT</conversion>"
        f"<value><![CDATA[{a}]]></value><scaleMin>0</scaleMin><scaleMax>1</scaleMax></partial>"
        for a in args)
    return ("<osc><enabled>1</enabled><send>1</send><receive>0</receive><feedback>0</feedback>"
            f"<connections>00001</connections><triggers>{trg}</triggers>{path}"
            f"<arguments>{argp}</arguments></osc>")

def node(ntype, props, values, messages=""):
    return (f"<node ID='{uid()}' type='{ntype}'><properties>{props}</properties>"
            f"<values>{values}</values><messages>{messages}</messages></node>")

# ---- controls ---------------------------------------------------------------
def label(name, text, x, y, w, h, tcol=(1,1,1,1), bg=0, size=14):
    props = (p_b("background", bg) + p_c("color",(0,0,0,1)) + p_f("cornerRadius",1) +
             p_i("font",0) + p_r("frame",x,y,w,h) + p_b("grabFocus",0) +
             p_b("interactive",0) + p_b("locked",0) + p_s("name",name) + p_i("orientation",0) +
             p_b("outline",0) + p_i("outlineStyle",1) + p_i("pointerPriority",0) + p_i("shape",1) +
             p_i("textAlignH",2) + p_i("textAlignV",2) + p_b("textClip",1) +
             p_c("textColor",tcol) + p_i("textLength",0) + p_i("textSize",size) + p_b("visible",1))
    vals = val("text", text) + val("touch","false")
    return node("LABEL", props, vals)

def xy(name, x, y, w, h, col, addr_xy, addr_gate):
    props = (p_b("background",1) + p_c("color",col) + p_f("cornerRadius",1) + p_b("cursor",1) +
             p_i("cursorDisplay",0) + p_r("frame",x,y,w,h) + p_b("grabFocus",1) +
             p_i("gridStepsX",10) + p_i("gridStepsY",10) + p_b("gridX",0) + p_b("gridY",0) +
             p_b("interactive",1) + p_b("lines",1) + p_i("linesDisplay",0) + p_b("lockX",0) +
             p_b("lockY",0) + p_b("locked",0) + p_s("name",name) + p_i("orientation",0) +
             p_b("outline",1) + p_i("outlineStyle",1) + p_i("pointerPriority",0) + p_i("response",0) +
             p_i("responseFactor",100) + p_i("shape",1) + p_b("visible",1))
    vals = val("touch","false") + val("x","0.5") + val("y","0.0")
    msgs = osc(addr_xy, ["x","y"], ["x","y"]) + osc(addr_gate, ["touch"], ["touch"])
    return node("XY", props, vals, msgs)

def button(name, x, y, w, h, col, addr):
    props = (p_b("background",1) + p_i("buttonType",0) + p_c("color",col) + p_f("cornerRadius",1) +
             p_r("frame",x,y,w,h) + p_b("grabFocus",1) + p_b("interactive",1) + p_b("locked",0) +
             p_s("name",name) + p_i("orientation",0) + p_b("outline",1) + p_i("outlineStyle",1) +
             p_i("pointerPriority",0) + p_b("press",1) + p_b("release",1) + p_i("shape",1) +
             p_b("valuePosition",0) + p_b("visible",1))
    vals = val("touch","false") + val("x","0")
    msgs = osc(addr, ["x"], ["x"])
    return node("BUTTON", props, vals, msgs)

# ---- layout -----------------------------------------------------------------
W, H = 1000, 700
TRACK_COLS = [(1,0.45,0.45,1), (0.45,0.9,0.55,1), (0.45,0.6,1,1), (1,0.75,0.35,1)]
DIM = [(c[0]*0.5, c[1]*0.5, c[2]*0.5, 1) for c in TRACK_COLS]  # buttons a bit darker

children = [label("title", "TRAFALGAR — remote", 15, 4, 400, 24, size=16)]
col_w, gap = 220, 26
for i in range(4):
    cx = 15 + i * (col_w + gap)
    c = TRACK_COLS[i]
    children.append(label(f"t{i}lbl", f"TRACK {i+1}", cx, 34, col_w, 26, tcol=c, size=14))
    children.append(xy(f"t{i}xy", cx, 66, col_w, 424, c, f"/track/{i}/xy", f"/track/{i}/gate"))
    # erase + clear buttons with labels on top
    bx2 = cx + col_w // 2 + 5
    bw = col_w // 2 - 5
    children.append(button(f"t{i}erase", cx, 500, bw, 90, DIM[i], f"/track/{i}/erase"))
    children.append(button(f"t{i}clear", bx2, 500, bw, 90, DIM[i], f"/track/{i}/clear"))
    children.append(label(f"t{i}elbl", "ERASE", cx, 500, bw, 90, size=13))
    children.append(label(f"t{i}clbl", "CLEAR", bx2, 500, bw, 90, size=13))

root_props = (p_b("background",1) + p_c("color",(0.08,0.08,0.1,1)) + p_f("cornerRadius",0) +
              p_r("frame",0,0,W,H) + p_b("grabFocus",0) + p_b("interactive",0) + p_b("locked",0) +
              p_s("name","trafalgar") + p_i("orientation",0) + p_b("outline",0) + p_i("outlineStyle",0) +
              p_i("pointerPriority",0) + p_i("shape",0) + p_b("visible",1))
# GROUPs are properties, values, children — no <messages> (matches TouchOSC output).
root = (f"<node ID='{uid()}' type='GROUP'><properties>{root_props}</properties>"
        f"<values>{val('touch','false')}</values>"
        f"<children>{''.join(children)}</children></node>")
xml = "<?xml version='1.0' encoding='UTF-8'?>\n<lexml version='3'>\n" + root + "\n</lexml>"

out_dir = "/Users/ac/developer/trafalgar/docs"
with open(os.path.join(out_dir, "trafalgar.tosc.xml"), "w") as f:
    f.write(xml)
with open(os.path.join(out_dir, "trafalgar.tosc"), "wb") as f:
    f.write(zlib.compress(xml.encode("utf-8")))

# round-trip sanity: decompress and re-parse
import xml.etree.ElementTree as ET
data = zlib.decompress(open(os.path.join(out_dir, "trafalgar.tosc"), "rb").read())
tree = ET.fromstring(data)
nodes = tree.findall(".//node")
print(f"OK  root tag={tree.tag}  nodes={len(nodes)}  xml_bytes={len(xml)}  tosc_bytes={os.path.getsize(os.path.join(out_dir,'trafalgar.tosc'))}")
addrs = [p.text for p in tree.iter('value') if p.text and p.text.startswith('/track/')]
print("addresses:", sorted(set(addrs)))
