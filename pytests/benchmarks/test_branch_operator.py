from datetime import datetime, timedelta, timezone

import bytewax.operators as op
import bytewax.operators.window as w
from bytewax.connectors.stdio import StdOutSink
from bytewax.dataflow import Dataflow
from bytewax.testing import BatchInput, cluster_main, run_main

BATCH_SIZE = 100_000
BATCH_COUNT = 10

clock_config = w.EventClockConfig(
    dt_getter=lambda x: x,
    wait_for_system_duration=timedelta(seconds=0),
)
window = w.TumblingWindow(
    align_to=datetime(2022, 1, 1, tzinfo=timezone.utc), length=timedelta(minutes=1)
)

flow = Dataflow("bench")
inp = op.input("in", flow, BatchInput(BATCH_COUNT, list(range(0, BATCH_SIZE))))
branch_out = op.branch("evens_and_odds", inp, lambda x: x / 2 == 0)
merged = op.merge("merge_streams", branch_out.trues, branch_out.falses)
op.output("stdout", merged, StdOutSink())


def test_branch_run_main(benchmark):
    benchmark(lambda: run_main(flow))


def test_branch_cluster_main(benchmark):
    benchmark(lambda: cluster_main(flow, addresses=["localhost:9999"], proc_id=0))
