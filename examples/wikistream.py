import json
import operator
from datetime import datetime, timedelta, timezone

# pip install aiohttp-sse-client
from aiohttp_sse_client.client import EventSource
from bytewax.connectors.files import FileOutput
from bytewax.dataflow import Dataflow
from bytewax.inputs import PartitionedInput, StatefulSource, batch_async
from bytewax.tracing import setup_tracing
from bytewax.window import SystemClockConfig, TumblingWindow

tracer = setup_tracing(
    log_level="TRACE",
)


async def _sse_agen(url):
    async with EventSource(url) as source:
        async for event in source:
            yield event.data


class WikiSource(StatefulSource):
    def __init__(self):
        agen = _sse_agen("https://stream.wikimedia.org/v2/stream/recentchange")
        # Gather up to 0.25 sec of or 1000 items.
        self._batcher = batch_async(agen, timedelta(seconds=0.25), 1000)

    def next_batch(self):
        return next(self._batcher)

    def snapshot(self):
        return None


class WikiStreamInput(PartitionedInput):
    def list_parts(self):
        return ["single-part"]

    def build_part(self, for_key, resume_state):
        assert for_key == "single-part"
        assert resume_state is None
        return WikiSource()


def initial_count(data_dict):
    return data_dict["server_name"], 1


def keep_max(max_count, new_count):
    new_max = max(max_count, new_count)
    # print(f"Just got {new_count}, old max was {max_count}, new max is {new_max}")
    return new_max, new_max


flow = Dataflow()
flow.input("inp", WikiStreamInput())
# "event_json"
flow.map("load_json", json.loads)
# {"server_name": "server.name", ...}
flow.map("initial_count", initial_count)
# ("server.name", 1)
flow.reduce_window(
    "sum",
    SystemClockConfig(),
    TumblingWindow(
        length=timedelta(seconds=2), align_to=datetime(2023, 1, 1, tzinfo=timezone.utc)
    ),
    operator.add,
)
# ("server.name", sum_per_window)
flow.stateful_map("keep_max", lambda: 0, keep_max)
# ("server.name", max_per_window)
flow.map("format", lambda x: (x[0], f"{x[0]}, {x[1]}"))
flow.output("out", FileOutput("wikifile.txt"))
