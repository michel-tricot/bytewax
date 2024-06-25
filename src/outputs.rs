//! Internal code for output.
//!
//! For a user-centric version of output, read the `bytewax.output`
//! Python module docstring. Read that first.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::rc::Rc;

use opentelemetry::KeyValue;
use pyo3::exceptions::PyTypeError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::Concatenate;
use timely::dataflow::operators::Map;
use timely::dataflow::operators::Operator;
use timely::dataflow::Scope;
use timely::dataflow::Stream;
use timely::progress::Timestamp;

use crate::errors::tracked_err;
use crate::errors::PythonException;
use crate::operators::ExtractKeyOp;
use crate::pyo3_extensions::TdPyAny;
use crate::pyo3_extensions::TdPyCallable;
use crate::recovery::*;
use crate::timely::*;
use crate::unwrap_any;
use crate::with_timer;

/// Represents a `bytewax.outputs.Sink` from Python.
#[derive(Clone)]
pub(crate) struct Sink(Py<PyAny>);

/// Do some eager type checking.
impl<'py> FromPyObject<'py> for Sink {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = ob.py();
        let abc = py.import_bound("bytewax.outputs")?.getattr("Sink")?;
        if !ob.is_instance(&abc)? {
            Err(tracked_err::<PyTypeError>(
                "sink must subclass `bytewax.outputs.Sink`",
            ))
        } else {
            Ok(Self(ob.to_object(py)))
        }
    }
}

impl IntoPy<Py<PyAny>> for Sink {
    fn into_py(self, _py: Python<'_>) -> Py<PyAny> {
        self.0
    }
}

impl ToPyObject for Sink {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        self.0.to_object(py)
    }
}

impl Sink {
    pub(crate) fn extract<'p, D>(&'p self, py: Python<'p>) -> PyResult<D>
    where
        D: FromPyObject<'p>,
    {
        self.0.extract(py)
    }
}

/// Represents a `bytewax.outputs.PartitionedOutput` from Python.
#[derive(Clone)]
pub(crate) struct FixedPartitionedSink(Py<PyAny>);

/// Do some eager type checking.
impl<'py> FromPyObject<'py> for FixedPartitionedSink {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = ob.py();
        let abc = py
            .import_bound("bytewax.outputs")?
            .getattr("FixedPartitionedSink")?;
        if !ob.is_instance(&abc)? {
            Err(tracked_err::<PyTypeError>(
                "fixed partitioned sink must subclass `bytewax.outputs.FixedPartitionedSink`",
            ))
        } else {
            Ok(Self(ob.to_object(py)))
        }
    }
}

impl FixedPartitionedSink {
    pub(crate) fn list_parts(&self, py: Python) -> PyResult<Vec<StateKey>> {
        self.0.call_method0(py, "list_parts")?.extract(py)
    }

    pub(crate) fn build_part(
        &self,
        py: Python,
        step_id: StepId,
        for_part: StateKey,
        resume_state: Option<PyObject>,
    ) -> PyResult<StatefulSinkPartition> {
        self.0
            .call_method1(py, "build_part", (step_id, for_part, resume_state))?
            .extract(py)
    }

    fn build_part_assigner(&self, py: Python) -> PyResult<PartitionAssigner> {
        Ok(PartitionAssigner(
            self.0.getattr(py, "part_fn")?.extract(py)?,
        ))
    }
}

/// Represents a `bytewax.outputs.StatefulSinkPartition` in Python.
pub(crate) struct StatefulSinkPartition(Py<PyAny>);

/// Do some eager type checking.
impl<'py> FromPyObject<'py> for StatefulSinkPartition {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = ob.py();
        let abc = py
            .import_bound("bytewax.outputs")?
            .getattr("StatefulSinkPartition")?;
        if !ob.is_instance(&abc)? {
            Err(tracked_err::<PyTypeError>(
                "stateful sink partition must subclass `bytewax.outputs.StatefulSinkPartition`",
            ))
        } else {
            Ok(Self(ob.to_object(py)))
        }
    }
}

/// This is a separate object than the bundle so we can use Python's
/// RC to clone it into the exchange closure.
struct PartitionAssigner(TdPyCallable);

impl PartitionAssigner {
    fn part_fn(&self, py: Python, key: &StateKey) -> PyResult<usize> {
        self.0.bind(py).call1((key.clone(),))?.extract()
    }
}

impl PartitionFn<StateKey> for PartitionAssigner {
    fn assign(&self, key: &StateKey) -> usize {
        // TODO: This is a hot inner GIL acquisition. This should be
        // refactored into the output operator itself, but because
        // we're piggy-backing on the pure-Timely recovery partitioned
        // IO operators, we don't have access to batches. TBH probably
        // this will go away with 2PC being worked into `stateful`
        // operator in Python.
        unwrap_any!(Python::with_gil(|py| self.part_fn(py, key))
            .reraise("error assigning output partition"))
    }
}

pub(crate) struct OutputState {
    step_id: StepId,

    // Shared references to LocalStateStore and StateStoreCache
    local_state_store: Option<Rc<RefCell<LocalStateStore>>>,
    state_store_cache: Rc<RefCell<StateStoreCache>>,

    // Builder function used each time we need to insert a partition into the state_store_cache.
    builder: Box<dyn Fn(Python, StateKey, Option<PyObject>) -> PyResult<StatefulSinkPartition>>,

    // Snapshots
    awoken: BTreeSet<StateKey>,
    snapshot_mode: SnapshotMode,
}

impl OutputState {
    pub fn init(
        py: Python,
        step_id: StepId,
        local_state_store: Option<Rc<RefCell<LocalStateStore>>>,
        state_store_cache: Rc<RefCell<StateStoreCache>>,
        sink: FixedPartitionedSink,
        snapshot_mode: SnapshotMode,
    ) -> PyResult<Self> {
        state_store_cache.borrow_mut().add_step(step_id.clone());
        let s_id = step_id.clone();
        let builder = move |py: Python<'_>, state_key, state| {
            let parts_list = unwrap_any!(sink.list_parts(py));
            assert!(
                parts_list.contains(&state_key),
                "State found for unknown key {} in the recovery store for {}. \
                Known partitions: {}. \
                Fixed partitions cannot change between executions, aborting.",
                state_key,
                &s_id,
                parts_list
                    .iter()
                    .map(|sk| format!("\"{}\"", sk.0))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            sink.build_part(py, s_id.clone(), state_key, state)
        };

        let mut this = Self {
            step_id,
            local_state_store,
            state_store_cache,
            snapshot_mode,
            awoken: BTreeSet::new(),
            builder: Box::new(builder),
        };

        if let Some(lss) = this.local_state_store.as_ref() {
            let snaps = lss.borrow_mut().get_snaps(py, &this.step_id)?;
            for (state_key, state) in snaps {
                this.insert(py, state_key, state)
            }
        }
        Ok(this)
    }

    pub fn insert(&mut self, py: Python, state_key: StateKey, state: Option<PyObject>) {
        let logic = (self.builder)(py, state_key.clone(), state).unwrap();
        self.state_store_cache
            .borrow_mut()
            .insert(&self.step_id, state_key, logic.0);
    }

    pub fn contains_key(&self, key: &StateKey) -> bool {
        self.state_store_cache
            .borrow()
            .contains_key(&self.step_id, key)
    }

    fn write_batch(&mut self, py: Python, key: &StateKey, batch: Vec<PyObject>) -> PyResult<()> {
        self.awoken.insert(key.clone());
        self.state_store_cache
            .borrow()
            .get(&self.step_id, key)
            .unwrap()
            .bind(py)
            .call_method1(intern!(py, "write_batch"), (batch,))
            .reraise_with(|| {
                format!(
                    "error calling `StatefulSinkPartition.write_batch` in \
                    step {} for partition {}",
                    self.step_id, key
                )
            })?;
        Ok(())
    }

    pub fn snap(&self, py: Python, key: StateKey, epoch: u64) -> PyResult<SerializedSnapshot> {
        let ssc = self.state_store_cache.borrow();
        let ser_change = ssc
            .get(&self.step_id, &key)
            // It's ok if there's no logic, because it might have been discarded
            // due to one of the `on_*` methods returning `IsComplete::Discard`.
            .map(|logic| -> PyResult<Vec<u8>> {
                let snap = logic
                    .call_method0(py, intern!(py, "snapshot"))
                    .reraise_with(|| {
                        format!(
                            "error calling `snapshot` in {} for key {}",
                            self.step_id, key
                        )
                    })?;

                let pickle = py.import_bound(intern!(py, "pickle"))?;
                let ser_snap = pickle
                    .call_method1(intern!(py, "dumps"), (snap.bind(py),))
                    .reraise("Error serializing snapshot")?;
                let ser_snap = ser_snap.downcast::<PyBytes>().unwrap();
                let ser_snap = ser_snap.as_bytes().to_vec();
                Ok(ser_snap)
            })
            .transpose()?;

        Ok(SerializedSnapshot::new(
            self.step_id.clone(),
            key,
            epoch,
            ser_change,
        ))
    }

    fn immediate_snapshots(&mut self, py: Python, epoch: u64) -> Vec<SerializedSnapshot> {
        let mut res = vec![];
        if self.snapshot_mode.immediate() {
            while let Some(key) = self.awoken.pop_first() {
                let snap = self
                    .snap(py, key, epoch)
                    .reraise("Error snapshotting FixedPartitionedSink")
                    .unwrap();
                res.push(snap)
            }
            if let Some(lss) = self.local_state_store.as_ref() {
                lss.borrow_mut().write_snapshots(res.clone());
            }
        }
        res
    }
    fn batch_snapshots(&mut self, py: Python, epoch: u64) -> Vec<SerializedSnapshot> {
        let mut res = vec![];
        if self.snapshot_mode.batch() {
            while let Some(key) = self.awoken.pop_first() {
                let snap = self
                    .snap(py, key, epoch)
                    .reraise("Error snapshotting FixedPartitionedSink")
                    .unwrap();
                res.push(snap)
            }
            if let Some(lss) = self.local_state_store.as_ref() {
                lss.borrow_mut().write_snapshots(res.clone());
            }
        }
        res
    }
}

pub(crate) trait PartitionedOutputOp<S>
where
    S: Scope,
{
    /// Write items to a partitioned output.
    ///
    /// Will manage assigning primary workers for all partitions and
    /// building them.
    ///
    /// This can't be unified into the recovery system output
    /// operators because they are stateless.
    fn partitioned_output(
        &self,
        py: Python,
        step_id: StepId,
        sink: FixedPartitionedSink,
        state: OutputState,
    ) -> PyResult<(ClockStream<S>, Stream<S, SerializedSnapshot>)>;
}

impl<S> PartitionedOutputOp<S> for Stream<S, TdPyAny>
where
    S: Scope<Timestamp = u64>,
{
    fn partitioned_output(
        &self,
        py: Python,
        step_id: StepId,
        sink: FixedPartitionedSink,
        state: OutputState,
    ) -> PyResult<(ClockStream<S>, Stream<S, SerializedSnapshot>)> {
        let this_worker = self.scope().w_index();
        let local_parts = sink.list_parts(py).reraise("error listing partitions")?;
        let all_parts = local_parts.into_broadcast(&self.scope(), S::Timestamp::minimum());
        let primary_updates = all_parts.assign_primaries(format!("{step_id}.assign_primaries"));

        let pf = sink.build_part_assigner(py)?;
        let routed_self = self
            .extract_key(step_id.clone())
            .partition(
                format!("{step_id}.partition"),
                &all_parts.map(|(part, _worker)| part),
                pf,
            )
            .route(format!("{step_id}.self_route"), &primary_updates);

        let op_name = format!("{step_id}.partitioned_output");
        let mut op_builder = OperatorBuilder::new(op_name.clone(), self.scope());

        let mut routed_input = op_builder.new_input(&routed_self, routed_exchange());

        let (mut clock_output, clock) = op_builder.new_output();
        // Create 2 separate outputs so that we can activate the session
        // both in eager and closing logics.
        let (mut immediate_snaps_output, immediate_snaps) = op_builder.new_output();
        let (mut batch_snaps_output, batch_snaps) = op_builder.new_output();
        // Then concatenate the two outputs
        let snaps = immediate_snaps.concatenate([batch_snaps]);

        let meter = opentelemetry::global::meter("bytewax");
        let item_inp_count = meter
            .u64_counter("item_inp_count")
            .with_description("number of items this step has ingested")
            .init();
        let write_batch_histogram = meter
            .f64_histogram("out_part_write_batch_duration_seconds")
            .with_description("`write_batch` duration in seconds")
            .init();
        let snapshot_histogram = meter
            .f64_histogram("snapshot_duration_seconds")
            .with_description("`snapshot` duration in seconds")
            .init();
        let labels = vec![
            KeyValue::new("step_id", step_id.0.to_string()),
            KeyValue::new("worker_index", this_worker.0.to_string()),
        ];

        op_builder.build(move |init_caps| {
            // Which partitions were written to in this epoch.
            // We only snapshot those.
            // let awoken: BTreeSet<StateKey> = BTreeSet::new();
            // let snaps = Vec::new();

            let mut routed_tmp = Vec::new();
            // First `StateKey` is partition, second is data routing.
            type PartToInBufferMap = BTreeMap<StateKey, Vec<(StateKey, TdPyAny)>>;
            let mut items_inbuf: BTreeMap<S::Timestamp, PartToInBufferMap> = BTreeMap::new();
            let mut ncater = EagerNotificator::new(init_caps, state);

            move |input_frontiers| {
                let _guard = tracing::debug_span!("operator", operator = op_name).entered();
                routed_input.for_each(|cap, incoming| {
                    let epoch = cap.time();
                    assert!(routed_tmp.is_empty());
                    incoming.swap(&mut routed_tmp);
                    for (worker, (part, (key, value))) in routed_tmp.drain(..) {
                        assert!(worker == this_worker);
                        items_inbuf
                            .entry(*epoch)
                            .or_insert_with(BTreeMap::new)
                            .entry(part)
                            .or_insert_with(Vec::new)
                            .push((key, value));
                    }

                    ncater.notify_at(*epoch);
                });

                ncater.for_each(
                    input_frontiers,
                    |caps, state| {
                        let clock_cap = &caps[0];
                        let snaps_cap = &caps[1];
                        let epoch = clock_cap.time();

                        // Writing happens eagerly in each epoch. We still use a
                        // notificator at all because we need to ensure that writes happen
                        // in epoch order.
                        if let Some(part_to_items) = items_inbuf.remove(epoch) {
                            Python::with_gil(|py| {
                                for (part_key, items) in part_to_items {
                                    // Build part if needed
                                    if !state.contains_key(&part_key) {
                                        state.insert(py, part_key.clone(), None);
                                    }

                                    // Remove the key from the items
                                    let batch: Vec<_> =
                                        items.into_iter().map(|(_k, v)| v.into()).collect();

                                    // Update metrics
                                    item_inp_count.add(batch.len() as u64, &labels);

                                    // And call write_batch
                                    with_timer!(
                                        write_batch_histogram,
                                        labels,
                                        unwrap_any!(state.write_batch(py, &part_key, batch))
                                    );
                                }

                                let mut snaps = with_timer!(
                                    snapshot_histogram,
                                    labels,
                                    state.immediate_snapshots(py, *epoch)
                                );
                                immediate_snaps_output
                                    .activate()
                                    .session(snaps_cap)
                                    .give_vec(&mut snaps);
                            });
                        };
                    },
                    |caps, state| {
                        let clock_cap = &caps[0];
                        let snaps_cap = &caps[2];
                        let epoch = clock_cap.time();

                        clock_output.activate().session(clock_cap).give(());
                        let mut snaps = Python::with_gil(|py| {
                            with_timer!(
                                snapshot_histogram,
                                labels,
                                state.batch_snapshots(py, *epoch)
                            )
                        });
                        batch_snaps_output
                            .activate()
                            .session(snaps_cap)
                            .give_vec(&mut snaps);
                    },
                );
            }
        });

        Ok((clock, snaps))
    }
}

/// Represents a `bytewax.outputs.DynamicSink` from Python.
#[derive(Clone)]
pub(crate) struct DynamicSink(Py<PyAny>);

/// Do some eager type checking.
impl<'py> FromPyObject<'py> for DynamicSink {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = ob.py();
        let abc = py.import_bound("bytewax.outputs")?.getattr("DynamicSink")?;
        if !ob.is_instance(&abc)? {
            Err(tracked_err::<PyTypeError>(
                "dynamic sink must subclass `bytewax.outputs.DynamicSink`",
            ))
        } else {
            Ok(Self(ob.to_object(py)))
        }
    }
}

impl DynamicSink {
    fn build(
        self,
        py: Python,
        step_id: &StepId,
        index: WorkerIndex,
        count: WorkerCount,
    ) -> PyResult<StatelessPartition> {
        self.0
            .call_method1(py, "build", (step_id.clone(), index.0, count.0))?
            .extract(py)
    }
}

/// Represents a `bytewax.outputs.StatelessSinkPartition` in Python.
struct StatelessPartition(Py<PyAny>);

/// Do some eager type checking.
impl<'py> FromPyObject<'py> for StatelessPartition {
    fn extract_bound(ob: &Bound<'py, PyAny>) -> PyResult<Self> {
        let py = ob.py();
        let abc = py
            .import_bound("bytewax.outputs")?
            .getattr("StatelessSinkPartition")?;
        if !ob.is_instance(&abc)? {
            Err(tracked_err::<PyTypeError>(
                "stateless sink partition must subclass `bytewax.outputs.StatelessSinkPartition`",
            ))
        } else {
            Ok(Self(ob.to_object(py)))
        }
    }
}

impl StatelessPartition {
    fn write_batch(&self, py: Python, items: Vec<PyObject>) -> PyResult<()> {
        let _ = self
            .0
            .call_method1(py, intern!(py, "write_batch"), (items,))?;
        Ok(())
    }

    fn close(&self, py: Python) -> PyResult<()> {
        let _ = self.0.call_method0(py, "close")?;
        Ok(())
    }
}

impl Drop for StatelessPartition {
    fn drop(&mut self) {
        unwrap_any!(Python::with_gil(|py| self
            .close(py)
            .reraise("error closing StatelessSinkPartition")));
    }
}

pub(crate) trait DynamicOutputOp<S>
where
    S: Scope,
{
    /// Write items to a dynamic output.
    ///
    /// Will manage automatically building sinks. All you have to do
    /// is pass in the definition.
    fn dynamic_output(
        &self,
        py: Python,
        step_id: StepId,
        sink: DynamicSink,
    ) -> PyResult<ClockStream<S>>;
}

impl<S> DynamicOutputOp<S> for Stream<S, TdPyAny>
where
    S: Scope<Timestamp = u64>,
{
    fn dynamic_output(
        &self,
        py: Python,
        step_id: StepId,
        sink: DynamicSink,
    ) -> PyResult<ClockStream<S>> {
        let worker_index = self.scope().w_index();
        let worker_count = self.scope().w_count();
        let mut part = Some(sink.build(py, &step_id, worker_index, worker_count)?);

        let meter = opentelemetry::global::meter("bytewax");
        let item_inp_count = meter
            .u64_counter("item_inp_count")
            .with_description("number of items this step has ingested")
            .init();
        let write_batch_histogram = meter
            .f64_histogram("out_part_write_batch_duration_seconds")
            .with_description("`write_batch` duration in seconds")
            .init();
        let labels = vec![
            KeyValue::new("step_id", step_id.0.to_string()),
            KeyValue::new("worker_index", worker_index.0.to_string()),
        ];

        let downstream = self.unary_frontier(Pipeline, &step_id.0, |_init_cap, _info| {
            let mut tmp_incoming: Vec<TdPyAny> = Vec::new();

            move |input, output| {
                part = part.take().and_then(|sink| {
                    input.for_each(|cap, incoming| {
                        assert!(tmp_incoming.is_empty());
                        incoming.swap(&mut tmp_incoming);

                        let mut output_session = output.session(&cap);

                        let batch: Vec<PyObject> = tmp_incoming
                            .split_off(0)
                            .into_iter()
                            .map(|item| item.into())
                            .collect();
                        item_inp_count.add(batch.len() as u64, &labels);
                        with_timer!(
                            write_batch_histogram,
                            &labels,
                            unwrap_any!(Python::with_gil(|py| sink
                                .write_batch(py, batch)
                                .reraise("error writing output batch")))
                        );

                        output_session.give(());
                    });

                    if input.frontier().is_empty() {
                        None
                    } else {
                        Some(sink)
                    }
                });
            }
        });

        Ok(downstream)
    }
}
