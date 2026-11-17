#!/usr/bin/env python3
"""Check rendered Kubernetes manifests against Rezn's supported subset (spec/kubernetes-subset.json).

    helm template ... | scripts/k8s_subset_check.py -          # report violations, exit 1 if any
    scripts/k8s_subset_check.py --markdown                      # print the tables used in docs/KUBERNETES-SUBSET.md

Requires PyYAML. Reference behaviour for `reznctl import --check`.
"""
import argparse
import base64
import binascii
import ipaddress
import json
import pathlib
import re
import sys

SUBSET = pathlib.Path(__file__).resolve().parent.parent / "spec" / "kubernetes-subset.json"

NAME = re.compile(r"^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$")
KEY = re.compile(r"^[-._a-zA-Z0-9]{1,253}$")
CPU = re.compile(r"^([0-9]+m|[0-9]+(\.[0-9]{1,3})?)$")
BYTES = re.compile(r"^[0-9]+(k|M|G|T|Ki|Mi|Gi|Ti)?$")
PORT_NAME = re.compile(r"^(?=.*[a-z])[a-z0-9]([-a-z0-9]{0,13}[a-z0-9])?$")
CRON_FIELD = re.compile(r"^[0-9*/,-]+$")
MAPS = {"labels", "stringmap", "base64map"}


def is_int(v):
    return isinstance(v, int) and not isinstance(v, bool)


def check_scalar(kind, value):
    """Return an error message, or None when value is of the given type."""
    if kind == "string":
        ok = isinstance(value, str)
    elif kind == "integer":
        ok = is_int(value)
    elif kind == "boolean":
        ok = isinstance(value, bool)
    elif kind == "name":
        ok = isinstance(value, str) and bool(NAME.match(value))
    elif kind == "key":
        ok = isinstance(value, str) and bool(KEY.match(value))
    elif kind == "cpu":
        ok = (isinstance(value, str) and bool(CPU.match(value))) or is_int(value)
    elif kind == "bytes":
        ok = (isinstance(value, str) and bool(BYTES.match(value))) or is_int(value)
    elif kind == "port":
        ok = is_int(value) and 1 <= value <= 65535
    elif kind == "portOrName":
        ok = (is_int(value) and 1 <= value <= 65535) or (isinstance(value, str) and bool(PORT_NAME.match(value)))
    elif kind == "cron":
        parts = value.split() if isinstance(value, str) else []
        ok = len(parts) == 5 and all(CRON_FIELD.match(p) for p in parts)
    elif kind == "cidr":
        try:
            ipaddress.IPv4Network(value, strict=True)
            ok = True
        except (ValueError, TypeError):
            ok = False
    elif kind == "path":
        ok = isinstance(value, str) and value.startswith("/")
    elif kind == "integerString":
        ok = isinstance(value, str) and bool(re.match(r"^-?[0-9]+$", value))
    else:
        return f"internal: unknown type {kind}"
    return None if ok else f"expected {kind}, got {value!r}"


class Checker:
    def __init__(self, subset):
        self.s = subset
        self.errors = []
        self.seen = set()  # ids of rules that matched, for coverage tests

    def err(self, ref, pointer, message):
        self.errors.append(f"{ref} {pointer or '/'}: {message}")

    def rules_for(self, kind):
        spec = self.s["kinds"][kind]
        rules = dict(self.s["metadata"])
        rules.update(spec["fields"])
        pod = spec.get("podSpec")
        if pod:
            for path, rule in self.s["podSpec"].items():
                rules[f"{pod}.{path}"] = rule
            for group in ("containers", "initContainers"):
                prefix = f"{pod}.{group}[]"
                for path, rule in self.s["container"].items():
                    rules[f"{prefix}.{path}"] = rule
            for path, rule in self.s["initContainer"].items():
                rules[f"{pod}.initContainers[].{path}"] = rule
        return rules

    def check_doc(self, index, doc):
        if not isinstance(doc, dict):
            self.err(f"#{index}", "", "document is not an object")
            return
        kind = doc.get("kind")
        name = (doc.get("metadata") or {}).get("name", "?")
        ref = f"#{index} {kind}/{name}"
        if kind not in self.s["kinds"]:
            self.err(ref, "/kind", f"unsupported kind {kind!r}")
            return
        expected = self.s["kinds"][kind]["apiVersion"]
        if doc.get("apiVersion") != expected:
            self.err(ref, "/apiVersion", f"expected {expected!r}, got {doc.get('apiVersion')!r}")
        rules = self.rules_for(kind)
        for key, value in doc.items():
            if key in ("apiVersion", "kind"):
                continue
            self.walk(ref, rules, key, f"/{key}", value)
        self.check_init_probes(ref, kind, doc)

    def check_init_probes(self, ref, kind, doc):
        """Only restartable init containers (sidecars) may have probes."""
        path = self.s["kinds"][kind].get("podSpec")
        if not path:
            return
        pod = doc
        for part in path.split("."):
            pod = pod.get(part) if isinstance(pod, dict) else None
        if not isinstance(pod, dict):
            return
        pointer = "/" + path.replace(".", "/")
        for i, container in enumerate(pod.get("initContainers") or []):
            if not isinstance(container, dict) or container.get("restartPolicy") == "Always":
                continue
            for probe in ("livenessProbe", "readinessProbe", "startupProbe"):
                if probe in container:
                    self.err(ref, f"{pointer}/initContainers/{i}/{probe}", "only sidecars (restartPolicy: Always) may have probes")

    def has_children(self, rules, path):
        return any(r.startswith(path + ".") or r.startswith(path + "[]") for r in rules)

    def walk(self, ref, rules, path, pointer, value):
        rule = rules.get(path)
        if rule is not None:
            self.apply(ref, rules, path, pointer, value, rule)
        elif isinstance(value, dict) and self.has_children(rules, path):
            for key, child in value.items():
                self.walk(ref, rules, f"{path}.{key}", f"{pointer}/{key}", child)
        elif isinstance(value, list) and self.has_children(rules, path):
            for i, child in enumerate(value):
                self.walk(ref, rules, f"{path}[]", f"{pointer}/{i}", child)
        else:
            self.err(ref, pointer, "unsupported field")

    def apply(self, ref, rules, path, pointer, value, rule):
        self.seen.add(id(rule))
        kind = rule["type"]
        if kind in MAPS:
            if not isinstance(value, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in value.items()):
                self.err(ref, pointer, f"expected {kind}")
            elif kind == "base64map":
                for k, v in value.items():
                    try:
                        base64.b64decode(v, validate=True)
                    except (binascii.Error, ValueError):
                        self.err(ref, f"{pointer}/{k}", "not base64")
            return
        if kind == "annotations":
            self.annotations(ref, pointer, value, template=False)
            return
        if kind == "templateMetadata":
            if not isinstance(value, dict):
                self.err(ref, pointer, "expected object")
                return
            for key, child in value.items():
                if key == "labels":
                    self.apply(ref, rules, path, f"{pointer}/labels", child, {"type": "labels"})
                elif key == "annotations":
                    self.annotations(ref, f"{pointer}/annotations", child, template=True)
                else:
                    self.err(ref, f"{pointer}/{key}", "unsupported field")
            return
        if kind == "probe":
            if not isinstance(value, dict):
                self.err(ref, pointer, "expected object")
                return
            probe_rules = {f"{path}.{p}": r for p, r in self.s["probe"].items()}
            handlers = [h for h in ("exec", "httpGet") if h in value]
            if len(handlers) != 1:
                self.err(ref, pointer, "a probe needs exactly one of exec or httpGet")
            for key, child in value.items():
                self.walk(ref, probe_rules, f"{path}.{key}", f"{pointer}/{key}", child)
            return
        if kind == "object":
            if not isinstance(value, dict):
                self.err(ref, pointer, "expected object")
                return
            for required in rule.get("requires", []):
                if required not in value:
                    self.err(ref, pointer, f"{required} is required")
            for key, child in value.items():
                self.walk(ref, rules, f"{path}.{key}", f"{pointer}/{key}", child)
            return
        if kind == "list":
            if not isinstance(value, list):
                self.err(ref, pointer, "expected list")
                return
            for i, child in enumerate(value):
                self.walk(ref, rules, f"{path}[]", f"{pointer}/{i}", child)
            return
        message = check_scalar(kind, value)
        if message:
            self.err(ref, pointer, message)
        elif "enum" in rule and value not in rule["enum"]:
            self.err(ref, pointer, f"{value!r} not in {rule['enum']}")

    def annotations(self, ref, pointer, value, template):
        if not isinstance(value, dict):
            self.err(ref, pointer, "expected object")
            return
        table = self.s["annotations"]
        for key, child in value.items():
            here = f"{pointer}/{key.replace('~', '~0').replace('/', '~1')}"
            rule = table.get(key) or (table["checksum/*"] if key.startswith("checksum/") else None)
            if rule is None or (template and not key.startswith("checksum/")):
                self.err(ref, here, "unsupported annotation")
                continue
            self.seen.add(id(rule))
            message = check_scalar(rule["type"], child)
            if message:
                self.err(ref, here, message)
            elif "enum" in rule and child not in rule["enum"]:
                self.err(ref, here, f"{child!r} not in {rule['enum']}")


def markdown(subset):
    def table(title, rules, prefix=""):
        rows = [f"### {title}", "", "| Path | Type | Values | Disposition | IR v2 | Notes |", "| --- | --- | --- | --- | --- | --- |"]
        for path, r in rules.items():
            values = ", ".join(f"`{json.dumps(v) if not isinstance(v, str) else v}`" for v in r.get("enum", [])) or "any"
            disposition = r.get("disposition", "supported")
            ir = f"`{r['ir']}`" if r.get("ir", "-") != "-" else "—"
            rows.append(f"| `{prefix}{path}` | {r['type']} | {values} | {disposition} | {ir} | {r.get('note', '')} |")
        return "\n".join(rows) + "\n"

    out = [table("Metadata (every kind)", subset["metadata"]), table("Annotations", subset["annotations"])]
    for kind, spec in subset["kinds"].items():
        pod = f" — pod spec at `{spec['podSpec']}`" if spec.get("podSpec") else ""
        out.append(table(f"{kind} (`{spec['apiVersion']}`) → `{spec['ir']}`{pod}", spec["fields"]))
    out.append(table("Pod spec", subset["podSpec"]))
    out.append(table("Containers and init containers", subset["container"]))
    out.append(table("Init containers only", subset["initContainer"]))
    out.append(table("Probes (liveness, readiness, startup)", subset["probe"]))
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("files", nargs="*", help="rendered YAML files, or - for stdin")
    parser.add_argument("--markdown", action="store_true", help="print the subset as Markdown tables")
    args = parser.parse_args()
    subset = json.loads(SUBSET.read_text())
    if args.markdown:
        print(markdown(subset), end="")
        return 0
    import yaml  # only needed for checking

    checker = Checker(subset)
    index = 0
    for name in args.files or ["-"]:
        text = sys.stdin.read() if name == "-" else pathlib.Path(name).read_text()
        for doc in yaml.safe_load_all(text):
            if doc is None:
                continue
            checker.check_doc(index, doc)
            index += 1
    for line in checker.errors:
        print(line)
    print(f"{index} documents, {len(checker.errors)} violations", file=sys.stderr)
    return 1 if checker.errors else 0


if __name__ == "__main__":
    sys.exit(main())
