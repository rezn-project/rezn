"""Tests for the IR v2 schema and examples and the Kubernetes subset (python3 -m unittest discover -s scripts).

Requires python3-jsonschema (Draft 2020-12) and python3-yaml.
"""
import copy
import importlib.util
import json
import pathlib
import unittest

import jsonschema
import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("k8s_subset_check", ROOT / "scripts" / "k8s_subset_check.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

IR_KINDS = {"replica_set", "ordered_replica_set", "job", "scheduled_job", "service", "volume_claim", "config",
            "secret", "ingress", "network_policy"}
BEGIN, END = "<!-- BEGIN GENERATED TABLES -->", "<!-- END GENERATED TABLES -->"


def load(path):
    return json.loads(path.read_text())


def pointer(error):
    return "".join(f"/{part}" for part in error.absolute_path)


class IrV2Schema(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        schema = load(ROOT / "spec" / "ir-v2.schema.json")
        jsonschema.Draft202012Validator.check_schema(schema)
        cls.validator = jsonschema.Draft202012Validator(schema)

    def test_examples_are_valid(self):
        files = sorted((ROOT / "examples" / "ir-v2").glob("*.json"))
        self.assertGreaterEqual(len(files), 6)
        for file in files:
            with self.subTest(file.name):
                errors = [f"{pointer(e)}: {e.message}" for e in self.validator.iter_errors(load(file))]
                self.assertEqual(errors, [])

    def test_invalid_examples_fail_where_expected(self):
        directory = ROOT / "examples" / "ir-v2" / "invalid"
        expected = load(directory / "expected.json")
        files = sorted(p for p in directory.glob("*.json") if p.name != "expected.json")
        self.assertEqual(sorted(expected), [p.name for p in files])
        for file in files:
            with self.subTest(file.name):
                at = expected[file.name]
                paths = [pointer(e) for e in self.validator.iter_errors(load(file))]
                self.assertTrue(paths, "should be invalid")
                self.assertTrue(any(p == at or p.startswith(at + "/") for p in paths), f"failed at {paths}, expected {at!r}")

    def test_semantic_examples_are_schema_valid(self):
        """Cross-reference rules need the validator: these pass the schema and fail in Rezn."""
        directory = ROOT / "examples" / "ir-v2" / "invalid-semantic"
        expected = load(directory / "expected.json")
        files = sorted(p for p in directory.glob("*.json") if p.name != "expected.json")
        self.assertEqual(sorted(expected), [p.name for p in files])
        for file in files:
            with self.subTest(file.name):
                self.assertEqual([pointer(e) for e in self.validator.iter_errors(load(file))], [])


class KubernetesSubset(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.subset = load(ROOT / "spec" / "kubernetes-subset.json")

    def rule_tables(self):
        s = self.subset
        tables = [s["metadata"], s["annotations"], s["podSpec"], s["container"], s["initContainer"], s["probe"]]
        return tables + [k["fields"] for k in s["kinds"].values()]

    def test_every_rule_is_typed_and_mapped(self):
        known = set(self.subset["types"]) | {"annotations", "templateMetadata", "probe", "integerString"}
        for kind, spec in self.subset["kinds"].items():
            self.assertIn(spec["ir"], IR_KINDS, kind)
        for table in self.rule_tables():
            for path, rule in table.items():
                self.assertIn(rule["type"], known, path)
                self.assertIsInstance(rule.get("ir"), str, path)

    def check_text(self, text):
        checker = check.Checker(self.subset)
        for i, doc in enumerate(d for d in yaml.safe_load_all(text) if d):
            checker.check_doc(i, doc)
        return checker

    def test_fixture_is_valid_and_uses_every_rule(self):
        checker = self.check_text((ROOT / "spec" / "testdata" / "kubernetes-valid.yaml").read_text())
        self.assertEqual(checker.errors, [])
        unused = [path for table in self.rule_tables() for path, rule in table.items() if id(rule) not in checker.seen]
        self.assertEqual(unused, [], "rules not exercised by spec/testdata/kubernetes-valid.yaml")

    def test_unsupported_content_is_reported(self):
        docs = [d for d in yaml.safe_load_all((ROOT / "spec" / "testdata" / "kubernetes-valid.yaml").read_text()) if d]
        deployment = next(d for d in docs if d["kind"] == "Deployment")
        def mutate(change):
            d = copy.deepcopy(deployment)
            change(d, d["spec"]["template"]["spec"])
            return d

        cases = {
            "/spec/strategy/type": mutate(lambda d, p: d["spec"].update(strategy={"type": "RollingUpdate"})),
            "/spec/template/spec/hostNetwork": mutate(lambda d, p: p.update(hostNetwork=True)),
            "/spec/template/spec/containers/0/readinessProbe/tcpSocket": mutate(
                lambda d, p: p["containers"][0].update(readinessProbe={"tcpSocket": {"port": 80}})),
            "/spec/template/spec/containers/0/securityContext/capabilities/add": mutate(
                lambda d, p: p["containers"][0]["securityContext"]["capabilities"].update(add=["NET_ADMIN"])),
            "/metadata/annotations/example.com~1x": mutate(lambda d, p: d["metadata"]["annotations"].update({"example.com/x": "y"})),
            "/spec/template/spec/volumes/5/emptyDir": mutate(lambda d, p: p["volumes"].append({"name": "x", "emptyDir": {}})),
            "/spec/template/spec/automountServiceAccountToken": mutate(lambda d, p: p.update(automountServiceAccountToken=True)),
            "/spec/template/spec/containers/0/resources/limits/memory": mutate(
                lambda d, p: p["containers"][0]["resources"]["limits"].update(memory="1.5Gi")),
            "/spec/template/spec/initContainers/0/startupProbe": mutate(
                lambda d, p: p["initContainers"][0].update(startupProbe={"exec": {"command": ["true"]}})),
            "/kind": dict(deployment, kind="DaemonSet"),
        }
        for at, doc in cases.items():
            with self.subTest(at):
                checker = self.check_text(yaml.safe_dump(doc))
                self.assertTrue(any(f" {at}:" in e for e in checker.errors), checker.errors)

    def test_documentation_tables_are_current(self):
        text = (ROOT / "docs" / "KUBERNETES-SUBSET.md").read_text()
        generated = text.split(BEGIN, 1)[1].split(END, 1)[0].strip()
        self.assertEqual(generated, check.markdown(self.subset).strip(),
                         "regenerate with scripts/k8s_subset_check.py --markdown")


if __name__ == "__main__":
    unittest.main()
