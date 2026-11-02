# Injected by vntl. Adds a `vntl_dump` command that writes every translatable
# line Ren'Py knows about (from .rpyc and .rpa, no decompiling) to JSON.
# Runs inside the game's own engine: Python 2.7 (Ren'Py 7) or 3 (Ren'Py 8).

init -999 python:

    def _vntl_nodes(t):
        # Ren'Py 8 merges one-line blocks into a TranslateSay node whose .block is
        # empty: the node itself is the line.
        if not t.block and isinstance(t, renpy.ast.Say):
            return [t]
        return t.block

    def _vntl_dump():
        import json, io
        import renpy.ast as ast

        text_type = type(u"")
        string_types = (str, text_type)

        ap = renpy.arguments.ArgumentParser(description="vntl dump")
        ap.add_argument("out")
        args = ap.parse_args()

        TOKEN = u"@@VNTL@@"
        tr = renpy.game.script.translator
        blocks = []

        for filename in sorted(tr.file_translates):
            for label, t in tr.file_translates[filename]:
                items = []
                for n in _vntl_nodes(t):
                    if isinstance(n, ast.Say):
                        old = n.what
                        n.what = TOKEN
                        try:
                            tpl = n.get_code()
                        finally:
                            n.what = old
                        items.append({"tpl": tpl, "who": n.who, "what": old})
                    else:
                        items.append({"code": n.get_code()})
                # Existing translations (tl/<lang>) of this block, used as a reference by vntl.
                tl = {}
                for lang in tr.languages:
                    lt = tr.language_translates.get((t.identifier, lang))
                    if lt is not None:
                        tl[lang] = [m.what for m in _vntl_nodes(lt) if isinstance(m, ast.Say)]
                blocks.append({"id": t.identifier, "file": t.filename, "line": t.linenumber,
                               "label": label or "", "items": items, "tl": tl})

        # Menu choices and character names go through Ren'Py's string table.
        strings = {}
        for n in renpy.game.script.all_stmts:
            if isinstance(n, ast.Menu):
                for item in n.items:
                    if item[0]:
                        strings.setdefault(item[0], {"file": n.filename, "line": n.linenumber, "kind": "menu"})

        names, nvl = {}, []
        for var, v in renpy.store.__dict__.items():
            if not isinstance(v, renpy.character.ADVCharacter):
                continue  # check first: some store objects (e.g. `style`) raise on unknown attributes
            if any(c.__name__ == "NVLCharacter" for c in type(v).__mro__):
                nvl.append(var)  # full-screen NVL text: pages, not a single text box
            name = v.name
            if isinstance(name, string_types) and name:
                names[var] = name
                strings.setdefault(name, {"file": "", "line": 0, "kind": "name"})

        # Japanese string literals in code. Screens and init-time code are the interface
        # ("ui"); literals in in-script Python ("code") keep their location so they are
        # only translated together with their scene.
        import re
        import ast as pyast
        cjk = re.compile(u"[぀-ヿ㐀-鿿]")
        lit = re.compile(r'''[uU]?("(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*')''')
        # Japanese file names are code, not text.
        path = re.compile(r"[/\\]|\.(png|jpe?g|webp|gif|ogg|mp3|wav|opus|flac|ttf|otf|ttc|webm|mp4|ogv|avi|rpy|txt|json)$", re.I)

        def literals(src):
            out = []
            for m in lit.finditer(src or u""):
                try:
                    s = pyast.literal_eval(u"u" + m.group(1))
                except Exception:
                    continue
                if isinstance(s, string_types) and cjk.search(s) and not path.search(s):
                    out.append(s)
            return out

        in_init = set()
        for n in renpy.game.script.all_stmts:
            if isinstance(n, ast.Init):
                for c in n.block:
                    in_init.add(id(c))

        defaults = {}
        for n in renpy.game.script.all_stmts:
            # `show text "..."` keeps its text in the image name; custom statements keep their source line.
            if isinstance(n, (ast.Show, ast.Scene)) and n.imspec:
                src = u" ".join(p for p in n.imspec[0] if isinstance(p, string_types))
            elif isinstance(n, ast.UserStatement):
                src = getattr(n, "line", None)
            else:
                src = None
            if src:
                for s in literals(src):
                    strings.setdefault(s, {"file": n.filename, "line": n.linenumber,
                                           "kind": "ui" if id(n) in in_init else "code"})
            # Any statement carrying Python code: python blocks, define/default, `image x = Text(...)`, ...
            src = getattr(getattr(n, "code", None), "source", None)
            if not isinstance(src, string_types):
                continue
            init = isinstance(n, (ast.EarlyPython, ast.Define, ast.Default, ast.Image)) or id(n) in in_init
            found = literals(src)
            for s in found:
                strings.setdefault(s, {"file": n.filename, "line": n.linenumber, "kind": "ui" if init else "code"})
            if isinstance(n, ast.Default) and found:
                defaults[n.store + "." + n.varname] = found

        def walk(node, out, depth=0):
            if node is None or depth > 100:
                return
            for e in getattr(node, "positional", None) or []:
                out.append(e)
            for kv in getattr(node, "keyword", None) or []:
                out.append(kv[1])
            for c in getattr(node, "children", None) or []:
                walk(c, out, depth + 1)
            for e in getattr(node, "entries", None) or []:
                walk(e[1], out, depth + 1)
            b = getattr(node, "block", None)
            if b is not None and hasattr(b, "children"):
                walk(b, out, depth + 1)

        for scr in list(renpy.display.screen.screens.values()):
            sast = getattr(scr, "ast", None)
            exprs = []
            walk(sast, exprs)
            loc = getattr(sast, "location", None) or ("", 0)
            for e in exprs:
                if isinstance(e, string_types):
                    for s in literals(e):
                        strings.setdefault(s, {"file": loc[0], "line": loc[1], "kind": "ui"})

        # Variables shown via [var] (e.g. the MC's name): their value after init and the
        # code lines that assign them, so vntl can translate the default value.
        interp = re.compile(r"\[([A-Za-z_][\w.]*)")
        shown = {}
        for b in blocks:
            for it in b["items"]:
                for v in interp.findall(it.get("what", u"")):
                    shown.setdefault(v, {"init": None, "assigns": []})
        for name in names.values():
            for v in interp.findall(name):
                shown.setdefault(v, {"init": None, "assigns": []})
        for v, info in shown.items():
            obj, attr = (renpy.store.persistent, v[11:]) if v.startswith("persistent.") else (renpy.store, v)
            if "." not in attr:
                val = getattr(obj, attr, None)
                if isinstance(val, string_types):
                    info["init"] = val
            pat = re.compile(r"^\s*(?:store\.|persistent\.)?%s\s*=(?!=)" % re.escape(attr))
            for n in renpy.game.script.all_stmts:
                src = getattr(getattr(n, "code", None), "source", None) if isinstance(n, (ast.Python, ast.EarlyPython)) else None
                for l in (src or u"").splitlines():
                    if pat.match(l):
                        info["assigns"].append({"file": n.filename, "line": n.linenumber, "code": l.strip()})

        # Dialogue layout, so vntl can tell when an English line no longer fits.
        layout = {}
        for sname in ("say_dialogue", "nvl_dialogue"):
            try:
                st = getattr(renpy.store.style, sname)
                layout[sname] = dict((p, getattr(st, p, None)) for p in ("size", "xmaximum", "line_leading"))
            except Exception:
                pass
        gui = getattr(renpy.store, "gui", None)
        layout["gui"] = dict((p, getattr(gui, p, None)) for p in
                             ("text_size", "dialogue_width", "nvl_text_width")) if gui is not None else {}
        layout["screen"] = [renpy.config.screen_width, renpy.config.screen_height]

        string_tl = dict((lang, dict(st.translations)) for lang, st in tr.strings.items()
                         if lang is not None and st.translations)

        # `translate <lang> python:` code, e.g. an official translation's font and text size for English.
        lang_python = {}
        for table in (tr.python, tr.early_block):
            for lang, nodes in table.items():
                for n in nodes:
                    for m in [n] + list(getattr(n, "block", None) or []):
                        src = getattr(getattr(m, "code", None), "source", None)
                        if lang is not None and isinstance(src, string_types) and src.strip():
                            lang_python.setdefault(lang, []).append(src)

        data = {"engine": "renpy", "blocks": blocks, "strings": [dict(text=k, **v) for k, v in strings.items()],
                "names": names, "string_tl": string_tl, "defaults": defaults, "shown_vars": shown, "nvl": nvl,
                "layout": layout, "lang_python": lang_python}
        with io.open(args.out, "w", encoding="utf-8") as f:
            # default=repr: a style value of an unexpected type must not break the dump
            f.write(text_type(json.dumps(data, ensure_ascii=False, indent=1, default=repr)))
        return False

    renpy.arguments.register_command("vntl_dump", _vntl_dump)
