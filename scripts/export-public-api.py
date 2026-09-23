#!/usr/bin/env python3
"""Render Sanscale's reachable declarations from rustdoc JSON, not source regexes.

cargo rustdoc --lib --all-features --locked -- -Z unstable-options --output-format json
python3 scripts/export-public-api.py target/doc/sanscale.json public-api.md

Requires a nightly rustdoc supporting JSON. Unknown forms fail instead of silently
omitting declarations. This deliberately does not duplicate upstream blanket impls.
"""
import argparse
import hashlib
import json
from pathlib import Path


class Inventory:
    def __init__(self, data):
        self.data = data
        self.index = data["index"]
        self.exports = {}
        self.walk(data["root"], "sanscale")
        self.methods = set()
        self.constants = set()
        self.trait_methods = set()
        self.impls = {}
        self.auto_traits = {}

    def item(self, ident):
        return self.index[str(ident)]

    def walk(self, ident, prefix):
        for child in self.item(ident)["inner"]["module"]["items"]:
            item = self.item(child)
            if item["visibility"] != "public":
                continue
            if "use" in item["inner"]:
                use = item["inner"]["use"]
                assert not use["is_glob"], "expand glob re-exports explicitly"
                child, name = use["id"], use["name"]
                item = self.item(child)
            else:
                name = item["name"]
            path = prefix + "::" + name
            if "module" in item["inner"]:
                self.walk(child, path)
            else:
                assert child not in self.exports, "handle aliased public paths explicitly"
                self.exports[child] = path

    def args(self, args):
        if not args:
            return ""
        angle = args["angle_bracketed"]
        out = []
        for arg in angle["args"]:
            kind, value = next(iter(arg.items()))
            out.append(self.ty(value) if kind == "type" else value)
        for constraint in angle["constraints"]:
            out.append(constraint["name"] + self.args(constraint["args"]) + " = "
                       + self.ty(constraint["binding"]["equality"]["type"]))
        return "<" + ", ".join(out) + ">" if out else ""

    def path(self, path):
        name = self.exports.get(path["id"], path["path"]).removeprefix("sanscale::")
        if "$crate" in name:
            name = "::".join(self.data["paths"][str(path["id"])]["path"])
        return name + self.args(path.get("args"))

    def bounds(self, bounds):
        out = []
        for bound in bounds:
            if "outlives" in bound:
                out.append(bound["outlives"])
            else:
                b = bound["trait_bound"]
                assert not b["generic_params"], "higher-ranked bounds need explicit support"
                assert b["modifier"] in ("none", "maybe")
                out.append(("?" if b["modifier"] == "maybe" else "") + self.path(b["trait"]))
        return " + ".join(out)

    def ty(self, ty):
        kind, value = next(iter(ty.items()))
        if kind in ("primitive", "generic"):
            return value
        if kind == "resolved_path":
            return self.path(value)
        if kind == "borrowed_ref":
            return "&" + ((value["lifetime"] + " ") if value["lifetime"] else "") + (
                "mut " if value["is_mutable"] else "") + self.ty(value["type"])
        if kind == "slice":
            return "[" + self.ty(value) + "]"
        if kind == "array":
            return "[" + self.ty(value["type"]) + "; " + value["len"] + "]"
        if kind == "tuple":
            return "(" + ", ".join(map(self.ty, value)) + ("," if len(value) == 1 else "") + ")"
        if kind == "dyn_trait":
            assert all(not t["generic_params"] for t in value["traits"])
            out = [self.path(t["trait"]) for t in value["traits"]]
            if value["lifetime"]:
                out.append(value["lifetime"])
            return "dyn " + " + ".join(out)
        if kind == "impl_trait":
            return "impl " + self.bounds(value)
        raise ValueError(f"unsupported type: {ty}")

    def generics(self, generics):
        out = []
        for param in generics["params"]:
            kind, value = next(iter(param["kind"].items()))
            if kind == "type" and value["is_synthetic"]:
                continue  # argument-position `impl Trait`, already present in its type
            name = param["name"]
            if kind == "lifetime":
                bounds = " + ".join(value["outlives"])
            else:
                assert kind == "type", f"unsupported generic: {param}"
                assert value["default"] is None
                bounds = self.bounds(value["bounds"])
            out.append(name + (": " + bounds if bounds else ""))
        return "<" + ", ".join(out) + ">" if out else ""

    def where(self, generics):
        out = []
        for predicate in generics["where_predicates"]:
            bound = predicate["bound_predicate"]
            assert not bound["generic_params"]
            out.append(self.ty(bound["type"]) + ": " + self.bounds(bound["bounds"]))
        return " where " + ", ".join(out) if out else ""

    def function(self, item, visibility="pub "):
        f = item["inner"]["function"]
        h, sig = f["header"], f["sig"]
        assert h["abi"] == "Rust" and not sig["is_c_variadic"]
        prefix = visibility + ("const " if h["is_const"] else "") + (
            "async " if h["is_async"] else "") + ("unsafe " if h["is_unsafe"] else "")
        args = []
        for name, ty in sig["inputs"]:
            text = self.ty(ty)
            args.append(text.replace("Self", "self") if name == "self" else name + ": " + text)
        start = prefix + "fn " + item["name"] + self.generics(f["generics"]) + "("
        end = ")" + (" -> " + self.ty(sig["output"]) if sig["output"] else "")
        end += self.where(f["generics"]) + ";"
        if len(start + ", ".join(args) + end) <= 100:
            return start + ", ".join(args) + end
        return start + "\n" + "".join("    " + arg + ",\n" for arg in args) + end

    def fields(self, ids, visibility=True):
        fields = []
        for ident in ids:
            field = self.item(ident)
            assert field["visibility"] in ("public", "default")
            fields.append("    " + ("pub " if visibility else "") + field["name"]
                          + ": " + self.ty(field["inner"]["struct_field"]) + ",")
        return "\n".join(fields)

    def declaration(self, item):
        kind, data = next(iter(item["inner"].items()))
        name = item["name"]
        if kind == "function":
            return self.function(item)
        head = "pub " + ("type" if kind == "type_alias" else kind) + " " + name
        head += self.generics(data["generics"]) + self.where(data["generics"])
        if kind == "type_alias":
            return head + " = " + self.ty(data["type"]) + ";"
        if kind == "struct":
            form = data["kind"]
            if "tuple" in form:
                values = [("pub " + self.ty(self.item(i)["inner"]["struct_field"]))
                          if i is not None else "/* private field */" for i in form["tuple"]]
                return head + "(" + ", ".join(values) + ");"
            plain = form["plain"]
            fields = self.fields(plain["fields"])
            if plain["has_stripped_fields"]:
                fields += ("\n" if fields else "") + "    /* private fields */"
            return head + " {\n" + fields + "\n}"
        if kind == "enum":
            assert not data["has_stripped_variants"]
            variants = []
            for ident in data["variants"]:
                variant = self.item(ident)
                v = variant["inner"]["variant"]
                text = variant["name"]
                if v["kind"] != "plain":
                    if "tuple" in v["kind"]:
                        text += "(" + ", ".join(self.ty(self.item(i)["inner"]["struct_field"])
                                                for i in v["kind"]["tuple"]) + ")"
                    else:
                        fields = v["kind"]["struct"]
                        assert not fields["has_stripped_fields"]
                        text += " {\n" + self.fields(fields["fields"], False) + "\n}"
                if v["discriminant"]:
                    text += " = " + v["discriminant"]["expr"]
                variants.append("    " + text.replace("\n", "\n    ") + ",")
            return head + " {\n" + "\n".join(variants) + "\n}"
        if kind == "trait":
            assert not data["is_auto"] and not data["is_unsafe"] and not data["bounds"]
            methods = []
            for ident in data["items"]:
                method = self.item(ident)
                self.trait_methods.add(ident)
                sig = self.function(method, "")
                if method["inner"]["function"]["has_body"]:
                    sig += " // default implementation provided"
                methods.append("    " + sig.replace("\n", "\n    "))
            return head + " {\n" + "\n".join(methods) + "\n}"
        raise ValueError(f"unsupported declaration: {kind}")

    def impl_header(self, impl):
        return ("unsafe " if impl["is_unsafe"] else "") + "impl" + self.generics(impl["generics"]) + " " + (
            self.path(impl["trait"]) + " for " if impl["trait"] else "") + self.ty(impl["for"]) + self.where(impl["generics"])

    def render(self):
        sections = []
        for ident, path in sorted(self.exports.items(), key=lambda kv: kv[1]):
            item = self.item(ident)
            kind, data = next(iter(item["inner"].items()))
            code = self.declaration(item)
            for iid in data.get("impls", []) + data.get("implementations", []):
                implementation = self.item(iid)
                impl = implementation["inner"]["impl"]
                if impl["is_synthetic"]:
                    marker = ("!" if impl["is_negative"] else "") + self.path(impl["trait"])
                    self.auto_traits.setdefault(path, []).append(marker)
                    continue
                if impl["blanket_impl"] is not None:
                    continue
                if impl["trait"]:
                    self.impls[iid] = impl
                    continue
                methods = []
                for mid in impl["items"]:
                    method = self.item(mid)
                    if method["visibility"] != "public":
                        continue
                    if "function" in method["inner"]:
                        self.methods.add(mid)
                        sig = self.function(method)
                    else:
                        self.constants.add(mid)
                        value = method["inner"]["assoc_const"]
                        sig = "pub const " + method["name"] + ": " + self.ty(value["type"]) + ";"
                    methods.append("    " + sig.replace("\n", "\n    "))
                if methods:
                    code += "\n\n" + self.impl_header(impl) + " {\n" + "\n".join(methods) + "\n}"
            span = item["span"]
            source = f'[{span["filename"]}:{span["begin"][0]}]({span["filename"]}#L{span["begin"][0]})'
            feature = ' — feature `perf-counters`' if '::profiling::' in path else ''
            docs = (item["docs"] or "").split("\n\n")[0].replace("[`", "`").replace("`]", "`")
            sections.append(f"## `{path}`{feature}\n\n{source}\n\n" + (docs + "\n\n" if docs else "")
                            + "```rust\n" + code + "\n```\n")
        trait_code = []
        for impl in sorted(self.impls.values(), key=self.impl_header):
            members = [self.function(self.item(i), "") for i in impl["items"]]
            # Provided upstream trait defaults are not declarations in Sanscale.
            if impl["provided_trait_methods"]:
                members.append("// Inherited defaults: " + ", ".join(impl["provided_trait_methods"]))
            body = "\n".join("    " + member.replace("\n", "\n    ") for member in members)
            trait_code.append(self.impl_header(impl) + (" {\n" + body + "\n}" if body else " {}"))
        sections.append("## Trait implementations (including derives)\n\n```rust\n"
                        + "\n\n".join(trait_code) + "\n```\n")
        sections.append("## Inferred auto traits\n\nCompiler/target-specific; `!` means a negative implementation.\n\n"
                        + "| Type | Auto traits |\n|---|---|\n" + "\n".join(
                            "| `" + path + "` | " + ", ".join("`" + t + "`" for t in sorted(traits)) + " |"
                            for path, traits in sorted(self.auto_traits.items())) + "\n")
        types = sum("function" not in self.item(i)["inner"] for i in self.exports)
        functions = len(self.exports) - types
        text = "\n".join(sections)
        digest = hashlib.sha256(text.encode()).hexdigest()
        header = f"""# Sanscale public API inventory

Generated from compiler-resolved rustdoc JSON, not a regex. Version **{self.data['crate_version']}**;
**all features** enabled. `profiling` is available only with `perf-counters`.

**{types} public types/traits/aliases · {functions} free functions · {len(self.methods)} inherent methods ·
{len(self.trait_methods)} trait method declarations · {len(self.constants)} associated constant.**
Explicit and derived trait implementations appear in the final section.

Scope: every reachable declaration defined by Sanscale, including macro-generated
items and re-exports from private modules. Public fields and enum payloads are
expanded; private fields are marked, not exposed. Function bodies are omitted.
Upstream blanket implementations and signatures of inherited standard-library
trait defaults are not duplicated; inherited defaults are named in the impl listing.
Inferred auto traits are listed separately; compiler-internal markers such as
`StructuralPartialEq`/`Freeze` are informative, not stable APIs to invoke.
This is a declaration inventory for review, not compilable replacement source.
`Cow` means `std::borrow::Cow`; `Range` means `std::ops::Range`.

Regenerate from the repository root (nightly rustdoc):

```sh
cargo rustdoc --lib --all-features --locked -- -Z unstable-options --output-format json
python3 scripts/export-public-api.py target/doc/sanscale.json public-api.md
```

Rustdoc JSON format: `{self.data['format_version']}`. Declaration fingerprint (SHA-256):
`{digest}`. Do not edit generated declarations by hand.

"""
        return header + text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("json", type=Path)
    parser.add_argument("markdown", type=Path)
    args = parser.parse_args()
    document = Inventory(json.loads(args.json.read_text())).render()
    args.markdown.write_text(document)
    print(f"Wrote {args.markdown}: {len(document.splitlines())} lines")


if __name__ == "__main__":
    main()
