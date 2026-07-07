//! PyO3 bindings - `pip install mtp2` gives a Rust-backed wheel exposing the
//! **same** MTP2 (ITU-T Q.703) signal-unit framing and link state machine the
//! crate ships.
//!
//! Compiled only with `--features python`; the default crate build is pyo3-free, so
//! `cargo add mtp2` / crates.io consumers pull zero pyo3. Two entry points share one
//! `add_contents()`:
//! * `#[pymodule] fn _mtp2` - the standalone wheel (maturin `module-name`).
//! * `pub fn register(py, parent)` - mount `mtp2` as a submodule of another
//!   extension, so a host can expose mtp2 without a second shared object.
//!
//! The Python surface mirrors the Rust one: `Fisu` / `Lssu` / `Msu` build and
//! encode signal units, `decode()` parses one back, and `Link` drives the Q.703
//! link state machine (feed it received SUs and timer ticks, pull SUs and events).

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyModule};

use crate::link::{AlignmentFailure, Event as CoreEvent, OutOfServiceReason};
use crate::retransmission::{PcrParams, RetransmissionMethod};
use crate::signal_unit::{SignalUnit, StatusIndication as CoreStatus, SuHeader};
use crate::{Mtp2Config, Mtp2Error as CoreError, Mtp2Link, Mtp2State as CoreState, MAX_MSU_BODY};

// ── Error mapping ───────────────────────────────────────────────────────────
create_exception!(
    mtp2,
    Mtp2Error,
    PyException,
    "MTP2 framing / state-machine error (ITU-T Q.703)."
);

fn mtp2_err(e: CoreError) -> PyErr {
    Mtp2Error::new_err(e.to_string())
}

// ── StatusIndication (Q.703 §11) ────────────────────────────────────────────
/// The six LSSU status indications. Integer values are the on-wire 3-bit codes
/// (`StatusIndication.SIOS == 3`).
#[pyclass(
    name = "StatusIndication",
    module = "mtp2._mtp2",
    eq,
    eq_int,
    from_py_object
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyStatus {
    SIO = 0,
    SIN = 1,
    SIE = 2,
    SIOS = 3,
    SIPO = 4,
    SIB = 5,
}

impl PyStatus {
    fn to_core(self) -> CoreStatus {
        match self {
            PyStatus::SIO => CoreStatus::OutOfAlignment,
            PyStatus::SIN => CoreStatus::NormalAlignment,
            PyStatus::SIE => CoreStatus::EmergencyAlignment,
            PyStatus::SIOS => CoreStatus::OutOfService,
            PyStatus::SIPO => CoreStatus::ProcessorOutage,
            PyStatus::SIB => CoreStatus::Busy,
        }
    }

    fn from_core(s: CoreStatus) -> Self {
        match s {
            CoreStatus::OutOfAlignment => PyStatus::SIO,
            CoreStatus::NormalAlignment => PyStatus::SIN,
            CoreStatus::EmergencyAlignment => PyStatus::SIE,
            CoreStatus::OutOfService => PyStatus::SIOS,
            CoreStatus::ProcessorOutage => PyStatus::SIPO,
            CoreStatus::Busy => PyStatus::SIB,
        }
    }
}

// ── Mtp2State (Q.703 §7 link state control) ─────────────────────────────────
/// The link's local state as tracked by [`Link`].
#[pyclass(
    name = "Mtp2State",
    module = "mtp2._mtp2",
    eq,
    eq_int,
    skip_from_py_object
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyState {
    OutOfService,
    NotAligned,
    Aligned,
    Proving,
    AlignedReady,
    AlignedNotReady,
    InService,
    ProcessorOutage,
}

impl PyState {
    fn from_core(s: CoreState) -> Self {
        match s {
            CoreState::OutOfService => PyState::OutOfService,
            CoreState::NotAligned => PyState::NotAligned,
            CoreState::Aligned => PyState::Aligned,
            CoreState::Proving => PyState::Proving,
            CoreState::AlignedReady => PyState::AlignedReady,
            CoreState::AlignedNotReady => PyState::AlignedNotReady,
            CoreState::InService => PyState::InService,
            CoreState::ProcessorOutage => PyState::ProcessorOutage,
        }
    }
}

// ── Signal units ────────────────────────────────────────────────────────────
/// A Fill-In Signal Unit (LI = 0): header only.
#[pyclass(name = "Fisu", module = "mtp2._mtp2", skip_from_py_object)]
#[derive(Clone)]
pub struct PyFisu {
    #[pyo3(get)]
    pub bsn: u8,
    #[pyo3(get)]
    pub bib: bool,
    #[pyo3(get)]
    pub fsn: u8,
    #[pyo3(get)]
    pub fib: bool,
}

#[pymethods]
impl PyFisu {
    #[new]
    #[pyo3(signature = (*, bsn = 127, bib = true, fsn = 127, fib = true))]
    fn new(bsn: u8, bib: bool, fsn: u8, fib: bool) -> Self {
        Self { bsn, bib, fsn, fib }
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let su =
            SignalUnit::fisu(header(self.bsn, self.bib, self.fsn, self.fib).map_err(mtp2_err)?);
        Ok(PyBytes::new(py, &su.encode().map_err(mtp2_err)?))
    }

    fn __repr__(&self) -> String {
        format!(
            "Fisu(bsn={}, bib={}, fsn={}, fib={})",
            self.bsn, self.bib as u8, self.fsn, self.fib as u8
        )
    }
}

/// A Link Status Signal Unit (LI = 1 or 2): carries a status indication.
#[pyclass(name = "Lssu", module = "mtp2._mtp2", skip_from_py_object)]
#[derive(Clone)]
pub struct PyLssu {
    #[pyo3(get)]
    pub status: PyStatus,
    #[pyo3(get)]
    pub bsn: u8,
    #[pyo3(get)]
    pub bib: bool,
    #[pyo3(get)]
    pub fsn: u8,
    #[pyo3(get)]
    pub fib: bool,
    #[pyo3(get)]
    pub extended: bool,
}

#[pymethods]
impl PyLssu {
    #[new]
    #[pyo3(signature = (status, *, bsn = 127, bib = true, fsn = 127, fib = true, extended = false))]
    fn new(status: PyStatus, bsn: u8, bib: bool, fsn: u8, fib: bool, extended: bool) -> Self {
        Self {
            status,
            bsn,
            bib,
            fsn,
            fib,
            extended,
        }
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let su = SignalUnit::Lssu {
            header: header(self.bsn, self.bib, self.fsn, self.fib).map_err(mtp2_err)?,
            status: self.status.to_core(),
            extended: self.extended,
        };
        Ok(PyBytes::new(py, &su.encode().map_err(mtp2_err)?))
    }

    fn __repr__(&self) -> String {
        format!(
            "Lssu({}, bsn={}, fsn={})",
            self.status.to_core(),
            self.bsn,
            self.fsn
        )
    }
}

/// A Message Signal Unit (LI = 3..=63): carries the MTP3 SIO + SIF.
#[pyclass(name = "Msu", module = "mtp2._mtp2", skip_from_py_object)]
#[derive(Clone)]
pub struct PyMsu {
    #[pyo3(get)]
    pub sio: u8,
    sif: Vec<u8>,
    #[pyo3(get)]
    pub bsn: u8,
    #[pyo3(get)]
    pub bib: bool,
    #[pyo3(get)]
    pub fsn: u8,
    #[pyo3(get)]
    pub fib: bool,
}

#[pymethods]
impl PyMsu {
    #[new]
    #[pyo3(signature = (sio, sif, *, bsn = 0, bib = true, fsn = 0, fib = true))]
    fn new(sio: u8, sif: Vec<u8>, bsn: u8, bib: bool, fsn: u8, fib: bool) -> Self {
        Self {
            sio,
            sif,
            bsn,
            bib,
            fsn,
            fib,
        }
    }

    /// The Service Information Field as `bytes`.
    #[getter]
    fn sif<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.sif)
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let su = SignalUnit::msu(
            header(self.bsn, self.bib, self.fsn, self.fib).map_err(mtp2_err)?,
            self.sio,
            self.sif.clone(),
        )
        .map_err(mtp2_err)?;
        Ok(PyBytes::new(py, &su.encode().map_err(mtp2_err)?))
    }

    fn __repr__(&self) -> String {
        format!(
            "Msu(sio={:#04x}, sif_len={}, bsn={}, fsn={})",
            self.sio,
            self.sif.len(),
            self.bsn,
            self.fsn
        )
    }
}

fn header(bsn: u8, bib: bool, fsn: u8, fib: bool) -> Result<SuHeader, CoreError> {
    SuHeader::new(bsn, bib, fsn, fib)
}

/// Convert a decoded core `SignalUnit` into the matching Python object.
fn su_to_py(py: Python<'_>, su: SignalUnit) -> PyResult<Py<PyAny>> {
    let h = su.header();
    let obj = match su {
        SignalUnit::Fisu { .. } => Bound::new(
            py,
            PyFisu {
                bsn: h.bsn,
                bib: h.bib,
                fsn: h.fsn,
                fib: h.fib,
            },
        )?
        .into_any(),
        SignalUnit::Lssu {
            status, extended, ..
        } => Bound::new(
            py,
            PyLssu {
                status: PyStatus::from_core(status),
                bsn: h.bsn,
                bib: h.bib,
                fsn: h.fsn,
                fib: h.fib,
                extended,
            },
        )?
        .into_any(),
        SignalUnit::Msu { sio, sif, .. } => Bound::new(
            py,
            PyMsu {
                sio,
                sif,
                bsn: h.bsn,
                bib: h.bib,
                fsn: h.fsn,
                fib: h.fib,
            },
        )?
        .into_any(),
    };
    Ok(obj.unbind())
}

/// Decode a signal unit (header + body, without the layer-1 flags/CRC) into a
/// [`Fisu`], [`Lssu`], or [`Msu`].
#[pyfunction]
fn decode(py: Python<'_>, data: &[u8]) -> PyResult<Py<PyAny>> {
    let su = SignalUnit::decode(data).map_err(mtp2_err)?;
    su_to_py(py, su)
}

// ── Event ────────────────────────────────────────────────────────────────────
/// An event raised by [`Link`]. `kind` names the event; `sio`/`sif` are set for a
/// delivered MSU and `detail` carries the reason for a failure/outage event.
#[pyclass(name = "Event", module = "mtp2._mtp2", skip_from_py_object)]
#[derive(Clone)]
pub struct PyEvent {
    #[pyo3(get)]
    pub kind: String,
    #[pyo3(get)]
    pub sio: Option<u8>,
    sif: Option<Vec<u8>>,
    #[pyo3(get)]
    pub detail: Option<String>,
}

#[pymethods]
impl PyEvent {
    /// The MSU Service Information Field, for a `kind == "msu"` event.
    #[getter]
    fn sif<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.sif.as_ref().map(|b| PyBytes::new(py, b))
    }

    fn __repr__(&self) -> String {
        match (&self.detail, self.sio) {
            (Some(d), _) => format!("Event({:?}, detail={:?})", self.kind, d),
            (None, Some(sio)) => format!("Event({:?}, sio={:#04x})", self.kind, sio),
            _ => format!("Event({:?})", self.kind),
        }
    }
}

fn oos_reason(r: OutOfServiceReason) -> &'static str {
    match r {
        OutOfServiceReason::Stopped => "stopped",
        OutOfServiceReason::ReceivedSios => "received_sios",
        OutOfServiceReason::AlignmentLost => "alignment_lost",
        OutOfServiceReason::SuermFailure => "suerm_failure",
        OutOfServiceReason::T6Expired => "t6_expired",
        OutOfServiceReason::T7Expired => "t7_expired",
    }
}

fn align_failure(f: AlignmentFailure) -> &'static str {
    match f {
        AlignmentFailure::AermTripped => "aerm_tripped",
        AlignmentFailure::ReceivedSios => "received_sios",
        AlignmentFailure::T2Expired => "t2_expired",
        AlignmentFailure::T3Expired => "t3_expired",
        AlignmentFailure::T1Expired => "t1_expired",
    }
}

fn event_to_py(e: CoreEvent) -> PyEvent {
    let mut ev = PyEvent {
        kind: String::new(),
        sio: None,
        sif: None,
        detail: None,
    };
    match e {
        CoreEvent::InService => ev.kind = "in_service".into(),
        CoreEvent::OutOfService(r) => {
            ev.kind = "out_of_service".into();
            ev.detail = Some(oos_reason(r).into());
        }
        CoreEvent::AlignmentFailed(f) => {
            ev.kind = "alignment_failed".into();
            ev.detail = Some(align_failure(f).into());
        }
        CoreEvent::Msu { sio, sif } => {
            ev.kind = "msu".into();
            ev.sio = Some(sio);
            ev.sif = Some(sif);
        }
        CoreEvent::RemoteProcessorOutage => ev.kind = "remote_processor_outage".into(),
        CoreEvent::RemoteProcessorRecovered => ev.kind = "remote_processor_recovered".into(),
        CoreEvent::RemoteBusy => ev.kind = "remote_busy".into(),
        CoreEvent::RemoteBusyEnded => ev.kind = "remote_busy_ended".into(),
        CoreEvent::RetransmissionRequested => ev.kind = "retransmission_requested".into(),
    }
    ev
}

// ── Link (the Q.703 state-machine driver) ────────────────────────────────────
/// The MTP2 signalling-link engine: feed it received SUs and timer ticks, pull
/// SUs to transmit and events for MTP3.
#[pyclass(name = "Link", module = "mtp2._mtp2")]
pub struct PyLink {
    inner: Mtp2Link,
}

#[pymethods]
impl PyLink {
    /// Build a link. `method` is `"basic"` (default) or `"pcr"`. Remaining keyword
    /// arguments override the Q.703 timer/monitor defaults (tick counts).
    #[new]
    #[pyo3(signature = (
        method = "basic",
        *,
        emergency = false,
        pcr_n1 = 127,
        pcr_n2 = 8000,
        t1 = None,
        t2 = None,
        t3 = None,
        t4_normal = None,
        t4_emergency = None,
        t6 = None,
        t7 = None,
        aerm_normal = None,
        aerm_emergency = None,
        suerm_threshold = None,
        suerm_interval = None,
        proving_attempts = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        method: &str,
        emergency: bool,
        pcr_n1: usize,
        pcr_n2: usize,
        t1: Option<u32>,
        t2: Option<u32>,
        t3: Option<u32>,
        t4_normal: Option<u32>,
        t4_emergency: Option<u32>,
        t6: Option<u32>,
        t7: Option<u32>,
        aerm_normal: Option<u32>,
        aerm_emergency: Option<u32>,
        suerm_threshold: Option<u32>,
        suerm_interval: Option<u32>,
        proving_attempts: Option<u32>,
    ) -> PyResult<Self> {
        let method = match method {
            "basic" => RetransmissionMethod::Basic,
            "pcr" => RetransmissionMethod::Pcr(PcrParams {
                n1: pcr_n1,
                n2: pcr_n2,
            }),
            other => {
                return Err(Mtp2Error::new_err(format!(
                    "unknown retransmission method {other:?} (expected \"basic\" or \"pcr\")"
                )))
            }
        };
        let d = Mtp2Config::default();
        let config = Mtp2Config {
            method,
            emergency,
            t1_aligned_ready: t1.unwrap_or(d.t1_aligned_ready),
            t2_not_aligned: t2.unwrap_or(d.t2_not_aligned),
            t3_aligned: t3.unwrap_or(d.t3_aligned),
            t4_proving_normal: t4_normal.unwrap_or(d.t4_proving_normal),
            t4_proving_emergency: t4_emergency.unwrap_or(d.t4_proving_emergency),
            t6_remote_congestion: t6.unwrap_or(d.t6_remote_congestion),
            t7_excessive_delay: t7.unwrap_or(d.t7_excessive_delay),
            aerm_threshold_normal: aerm_normal.unwrap_or(d.aerm_threshold_normal),
            aerm_threshold_emergency: aerm_emergency.unwrap_or(d.aerm_threshold_emergency),
            suerm_threshold: suerm_threshold.unwrap_or(d.suerm_threshold),
            suerm_decrement_interval: suerm_interval.unwrap_or(d.suerm_decrement_interval),
            proving_attempts: proving_attempts.unwrap_or(d.proving_attempts),
        };
        Ok(Self {
            inner: Mtp2Link::new(config),
        })
    }

    /// The current link state.
    #[getter]
    fn state(&self) -> PyState {
        PyState::from_core(self.inner.state())
    }

    /// Start initial alignment.
    fn start(&mut self) {
        self.inner.start();
    }

    /// Take the link out of service.
    fn stop(&mut self) {
        self.inner.stop();
    }

    /// Request (or clear) emergency alignment.
    fn set_emergency(&mut self, emergency: bool) {
        self.inner.set_emergency(emergency);
    }

    /// Signal a local processor outage.
    fn local_processor_outage(&mut self) {
        self.inner.local_processor_outage();
    }

    /// Signal that the local processor has recovered.
    fn local_processor_recovered(&mut self) {
        self.inner.local_processor_recovered();
    }

    /// Signal local congestion (the link sends SIB until cleared).
    fn local_busy(&mut self) {
        self.inner.local_busy();
    }

    /// Signal that local congestion has cleared.
    fn local_busy_ended(&mut self) {
        self.inner.local_busy_ended();
    }

    /// Queue an MSU (SIO + SIF) for transmission.
    fn submit_msu(&mut self, sio: u8, sif: Vec<u8>) -> PyResult<()> {
        self.inner.submit_msu(sio, sif).map_err(mtp2_err)
    }

    /// Pull the next signal unit to transmit, or `None` if the link is powered off.
    fn poll_transmit(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        match self.inner.poll_transmit() {
            Some(su) => Ok(Some(su_to_py(py, su)?)),
            None => Ok(None),
        }
    }

    /// Feed a received signal unit (raw header+body bytes, e.g. from `su.encode()`).
    fn handle_su(&mut self, data: &[u8]) -> PyResult<()> {
        let su = SignalUnit::decode(data).map_err(mtp2_err)?;
        self.inner.handle_su(&su);
        Ok(())
    }

    /// Report a signal unit the framer received with a bad CRC (feeds AERM/SUERM).
    fn handle_corrupted_su(&mut self) {
        self.inner.handle_corrupted_su();
    }

    /// Advance the link's timers by one tick.
    fn tick(&mut self) {
        self.inner.tick();
    }

    /// Pull the next event raised by the link, or `None`.
    fn poll_event(&mut self) -> Option<PyEvent> {
        self.inner.poll_event().map(event_to_py)
    }

    fn __repr__(&self) -> String {
        format!("Link(state={})", self.inner.state())
    }
}

// ── Module wiring ───────────────────────────────────────────────────────────
fn add_contents(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("Mtp2Error", m.py().get_type::<Mtp2Error>())?;
    m.add_class::<PyStatus>()?;
    m.add_class::<PyState>()?;
    m.add_class::<PyFisu>()?;
    m.add_class::<PyLssu>()?;
    m.add_class::<PyMsu>()?;
    m.add_class::<PyEvent>()?;
    m.add_class::<PyLink>()?;
    m.add_function(wrap_pyfunction!(decode, m)?)?;
    // Protocol constants.
    m.add("MAX_MSU_BODY", MAX_MSU_BODY)?;
    Ok(())
}

/// Standalone wheel entry point (maturin `module-name = "mtp2._mtp2"`).
#[pymodule]
fn _mtp2(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_contents(m)
}

/// Embedding entry point: build an `mtp2` submodule and attach it to `parent`, so
/// a host extension can expose mtp2 without a second shared object.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "mtp2")?;
    add_contents(&m)?;
    parent.setattr("mtp2", &m)?;
    Ok(())
}
