import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("perf_report", Path(__file__).resolve().parents[1] / "perf-report.py")
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


def fixture(mode="timing"):
    return {"schema":1,"suite_version":1,"tier":"quick","warmup":2,"mode":mode,
            "metadata":{"fonts":[],"cpu":"test cpu","debug_assertions":False,
                        "source_sha256":"abc","benchmark_sha256":"def"},
            "corpora":{"hash":"fixture"},"pending_features":["span tests"],
            "results":[{"name":"a <b>","params":{"bytes":4},"units":2,
                        "samples":[{"total_ns":n,"work":{"shape_calls":2},
                                    "memory":{"alloc_calls":4}}
                                   for n in (10000,20000,30000)]}]}


class ReportTests(unittest.TestCase):
    def test_normalizes_per_operation_and_percentile(self):
        dist = report.distribution(fixture()["results"][0])
        self.assertEqual(dist["p50"],10000)
        self.assertEqual(dist["p95"],15000)
        self.assertEqual(report.median_fields(fixture()["results"][0],"work")["shape_calls"],1)

    def test_cannot_use_instrumented_latency_as_baseline(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)/"work.json"
            path.write_text(json.dumps(fixture("work")))
            with self.assertRaisesRegex(ValueError,"expected"):
                report.load(path,"timing")

    def test_environment_and_companion_validation(self):
        a,b=fixture(),fixture()
        b["metadata"]["cpu"]="other"
        self.assertIn("metadata.cpu",report.environmental_differences(a,b))
        with self.assertRaisesRegex(ValueError,"companion mismatch"):
            report.checked_work(a,b)
        b=fixture("work")
        b["metadata"]["source_sha256"]="changed"
        with self.assertRaisesRegex(ValueError,"source_sha256"):
            report.checked_work(a,b)

    def test_new_and_removed_cases_are_not_silently_dropped(self):
        a,b=fixture(),fixture()
        extra=copy.deepcopy(b["results"][0]);extra["name"]="new case"
        b["results"].append(extra)
        page,_,_=report.render(a,b)
        self.assertIn("new case",page)
        self.assertIn("added",page)

    def test_mismatched_workload_refused(self):
        a,b=fixture(),fixture()
        b["results"][0]["params"]["bytes"]=100
        with self.assertRaisesRegex(ValueError,"inputs/units changed"):
            report.render(a,b)

    def test_reports_regression_and_escapes_input(self):
        a,b=fixture(),fixture()
        for s in b["results"][0]["samples"]:
            s["total_ns"]*=2
        page,_,regressions=report.render(a,b,fixture("work"),fixture("work"))
        self.assertEqual(regressions,["a <b>"])
        self.assertIn("a &lt;b&gt;",page)
        self.assertNotIn("<b>",page)
        self.assertIn("shape_calls",page)
        self.assertIn("2.000×",page)

    def test_rare_work_is_visible_even_with_zero_median(self):
        work=fixture("work")
        for i,s in enumerate(work["results"][0]["samples"]):
            s["work"]={"rare_eviction":100 if i==2 else 0}
        page,_,_=report.render(fixture(),before_work=work)
        self.assertIn("rare_eviction",page)
        self.assertIn("0 [0 … 50]",page)

    def test_sample_history_mismatch_refused(self):
        a,b=fixture(),fixture()
        b["results"][0]["samples"].pop()
        with self.assertRaisesRegex(ValueError,"sample count"):
            report.render(a,b)

    def test_baseline_only(self):
        page,_,regressions=report.render(fixture(),before_work=fixture("work"))
        self.assertIn("performance baseline",page)
        self.assertEqual(regressions,[])
        self.assertNotIn("New p50",page)


if __name__ == "__main__":
    unittest.main()
