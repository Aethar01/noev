//! Direct tests for the QIR-shaped Runtime C ABI (`spec/runtime.md` §4).

use std::cell::RefCell;
#[cfg(feature = "ketrs")]
use std::ffi::c_void;
use std::ptr;
use std::rc::Rc;

use noev_runtime::cabi::*;

fn error_string(rt: *const NoevRuntime) -> String {
    let mut len = 0usize;
    let ptr = unsafe { noev_rt_error_message(rt, &mut len) };
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(slice).into_owned()
}

#[test]
fn export_recording_mode_and_errors() {
    unsafe {
        let rt = noev_rt_new_export(42);
        assert!(!rt.is_null());
        assert!(!noev_rt_has_error(rt));

        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        assert!(noev_qis_h(rt, q0));
        noev_rt_ctrl_push(rt, &q0, 1);
        assert!(noev_qis_x(rt, q1));
        noev_rt_ctrl_pop(rt);

        let mut r0 = NoevResult(0);
        let mut r1 = NoevResult(0);
        assert!(noev_qis_mz(rt, q0, &mut r0));
        assert!(noev_qis_mz(rt, q1, &mut r1));
        assert!(noev_rt_result_is_clbit(r0));
        assert!(noev_rt_result_is_clbit(r1));
        assert_eq!(noev_rt_result_clbit_index(r0), 0);
        assert_eq!(noev_rt_result_clbit_index(r1), 1);
        assert!(noev_rt_measured(rt));

        // Assertions and custom error reporting.
        let msg = b"custom boom";
        assert!(noev_rt_assert(rt, true, msg.as_ptr(), msg.len()));
        assert!(!noev_rt_has_error(rt));
        assert!(!noev_rt_assert(rt, false, msg.as_ptr(), msg.len()));
        assert!(noev_rt_has_error(rt));
        assert_eq!(error_string(rt), "custom boom");
        noev_rt_clear_error(rt);
        assert!(!noev_rt_has_error(rt));

        noev_rt_set_error(rt, b"set directly".as_ptr(), 12);
        assert_eq!(error_string(rt), "set directly");
        noev_rt_clear_error(rt);

        // Null pointer safety.
        assert!(!noev_rt_qubit_allocate(ptr::null_mut(), &mut q0));
        assert!(!noev_rt_qubit_allocate(rt, ptr::null_mut()));
        assert!(noev_rt_has_error(rt));

        noev_rt_free(rt);
        noev_rt_free(ptr::null_mut());
    }
}

#[test]
fn output_write_and_flush() {
    let stdout = Rc::new(RefCell::new(Vec::<u8>::new()));
    let stderr = Rc::new(RefCell::new(Vec::<u8>::new()));

    struct SharedBuf(Rc<RefCell<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut rt = NoevRuntime::new_owned(Box::new(noev_runtime::backend::Exporter::default()), 7);
    rt.set_output(Rc::new(RefCell::new(Output {
        stdout: Box::new(SharedBuf(stdout.clone())),
        stderr: Box::new(SharedBuf(stderr.clone())),
    })));
    unsafe {
        assert!(noev_rt_write(&mut rt, false, b"hi\n".as_ptr(), 3));
        assert!(noev_rt_write(&mut rt, true, b"err\n".as_ptr(), 4));
        assert!(noev_rt_flush(&mut rt));
    }
    assert_eq!(&*stdout.borrow(), b"hi\n");
    assert_eq!(&*stderr.borrow(), b"err\n");
}

#[cfg(feature = "ketrs")]
#[test]
fn ketrs_gates_ctrl_adj_with_and_inspection() {
    use std::f64::consts::PI;

    unsafe {
        let rt = noev_rt_new_ketrs(12345);
        assert!(!rt.is_null());

        // Allocate 2 qubits, create Bell state |Φ+⟩ = (|00⟩ + |11⟩)/√2.
        let mut q0 = NoevQubit(0);
        let mut q1 = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut q0));
        assert!(noev_rt_qubit_allocate(rt, &mut q1));
        assert!(noev_qis_h(rt, q0));
        noev_rt_ctrl_push(rt, &q0, 1);
        assert!(noev_qis_x(rt, q1));
        noev_rt_ctrl_pop(rt);

        // Inspect amplitudes via C ABI.
        let qs = [q0, q1];
        let mut amps = [NoevComplex::default(); 4];
        assert!(noev_rt_amplitudes(rt, qs.as_ptr(), 2, amps.as_mut_ptr(), 4));
        let inv_sqrt2 = std::f64::consts::FRAC_1_SQRT_2;
        assert!((amps[0].re - inv_sqrt2).abs() < 1e-12);
        assert!(amps[1].re.abs() < 1e-12);
        assert!(amps[2].re.abs() < 1e-12);
        assert!((amps[3].re - inv_sqrt2).abs() < 1e-12);

        // Exact expectation ⟨Z0 Z1⟩ = +1.0 on |Φ+⟩.
        let factors = [
            NoevPauliFactor {
                qubit: q0,
                pauli: 2,
            },
            NoevPauliFactor {
                qubit: q1,
                pauli: 2,
            },
        ];
        let term = NoevPauliTerm {
            coeff: 1.0,
            factors: factors.as_ptr(),
            len: 2,
        };
        let mut exp = 0.0;
        assert!(noev_rt_expectation_exact(rt, &term, 1, &mut exp));
        assert!((exp - 1.0).abs() < 1e-12);

        // Uncompute Bell state via `adj` and verify zero amplitude = 1.0.
        noev_rt_adj_start(rt);
        assert!(noev_qis_h(rt, q0));
        noev_rt_ctrl_push(rt, &q0, 1);
        assert!(noev_qis_x(rt, q1));
        noev_rt_ctrl_pop(rt);
        assert!(noev_rt_adj_end(rt));

        let mut zero = NoevComplex::default();
        assert!(noev_rt_zero_amplitude(rt, &mut zero));
        assert!((zero.re - 1.0).abs() < 1e-12 && zero.im.abs() < 1e-12);

        // `with`/`do` with ancilla allocation + uncomputation under outer `ctrl`.
        assert!(noev_qis_x(rt, q0)); // q0 = |1⟩
        let depth = noev_rt_ctrl_depth(rt);
        noev_rt_ctrl_push(rt, &q0, 1);
        noev_rt_with_start(rt);
        let mut anc = NoevQubit(0);
        assert!(noev_rt_qubit_allocate(rt, &mut anc));
        assert!(noev_qis_x(rt, anc)); // anc = |1⟩ (not controlled by q0)
        noev_rt_with_do(rt);
        // Under outer ctrl q0 AND anc: flip q1.
        noev_rt_ctrl_push(rt, &anc, 1);
        assert!(noev_qis_x(rt, q1));
        noev_rt_ctrl_pop(rt);
        assert!(noev_rt_with_end(rt)); // uncomputes anc to |0⟩ and releases it!
        noev_rt_ctrl_restore(rt, depth);

        // Exercise remaining core gates (Y, Z, S, S†, T, T†, Rx, Ry, Rz, P, U, U†, SWAP, unitary).
        assert!(noev_qis_y(rt, q1));
        assert!(noev_qis_y(rt, q1));
        assert!(noev_qis_z(rt, q1));
        assert!(noev_qis_z(rt, q1));
        assert!(noev_qis_s(rt, q1));
        assert!(noev_qis_s_adj(rt, q1));
        assert!(noev_qis_t(rt, q1));
        assert!(noev_qis_t_adj(rt, q1));
        assert!(noev_qis_rx(rt, PI / 3.0, q1));
        assert!(noev_qis_rx(rt, -PI / 3.0, q1));
        assert!(noev_qis_ry(rt, PI / 4.0, q1));
        assert!(noev_qis_ry(rt, -PI / 4.0, q1));
        assert!(noev_qis_rz(rt, PI / 5.0, q1));
        assert!(noev_qis_rz(rt, -PI / 5.0, q1));
        assert!(noev_qis_p(rt, PI / 6.0, q1));
        assert!(noev_qis_p(rt, -PI / 6.0, q1));
        assert!(noev_qis_u(rt, 0.3, 0.4, 0.5, q1));
        assert!(noev_qis_u_adj(rt, 0.3, 0.4, 0.5, q1));
        assert!(noev_qis_swap(rt, q0, q1));
        assert!(noev_qis_swap(rt, q0, q1));
        // Identity 2x2 unitary.
        let id_mat = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        assert!(noev_qis_unitary(rt, &q1, 1, id_mat.as_ptr(), 8, false));

        // Release fails on |1⟩, succeeds after reset.
        assert!(!noev_rt_qubit_release(rt, q1));
        assert!(noev_rt_has_error(rt));
        assert!(error_string(rt).contains("|0⟩"));
        noev_rt_clear_error(rt);

        assert!(noev_rt_qubit_reset(rt, q1));
        assert!(noev_rt_qubit_release(rt, q1));

        // Measure q0 (in |1⟩).
        let mut res = NoevResult(0);
        assert!(noev_qis_mz(rt, q0, &mut res));
        assert!(!noev_rt_result_is_clbit(res));
        assert_eq!(noev_rt_result_read(rt, res), 1);

        noev_rt_free(rt);
    }
}

#[cfg(feature = "ketrs")]
#[test]
fn ketrs_sample_run_circuit_and_gate_level() {
    #[derive(Default)]
    struct BellEnv {
        r0: NoevResult,
        r1: NoevResult,
    }
    #[derive(Default)]
    struct Tally {
        c00: usize,
        c11: usize,
        other: usize,
    }

    extern "C" fn bell_body(rt: *mut NoevRuntime, env: *mut c_void) -> bool {
        let env = unsafe { &mut *(env as *mut BellEnv) };
        unsafe {
            let mut q0 = NoevQubit(0);
            let mut q1 = NoevQubit(0);
            if !noev_rt_qubit_allocate(rt, &mut q0) || !noev_rt_qubit_allocate(rt, &mut q1) {
                return false;
            }
            if !noev_qis_h(rt, q0) {
                return false;
            }
            noev_rt_ctrl_push(rt, &q0, 1);
            let ok = noev_qis_x(rt, q1);
            noev_rt_ctrl_pop(rt);
            if !ok {
                return false;
            }
            noev_qis_mz(rt, q0, &mut env.r0) && noev_qis_mz(rt, q1, &mut env.r1)
        }
    }

    extern "C" fn collect(rt: *mut NoevRuntime, count: usize, env: *mut c_void) -> bool {
        let (bell, tally) = unsafe { &mut *(env as *mut (&mut BellEnv, Tally)) };
        let b0 = unsafe { noev_rt_result_read(rt, bell.r0) };
        let b1 = unsafe { noev_rt_result_read(rt, bell.r1) };
        match (b0, b1) {
            (0, 0) => tally.c00 += count,
            (1, 1) => tally.c11 += count,
            _ => tally.other += count,
        }
        true
    }

    for is_circuit in [true, false] {
        unsafe {
            let rt = noev_rt_new_ketrs(99);
            let mut bell = BellEnv::default();
            let bell_ptr = &mut bell as *mut BellEnv;
            let mut cb_state = (&mut *bell_ptr, Tally::default());
            let ok = noev_rt_sample_run(
                rt,
                200,
                is_circuit,
                bell_body,
                bell_ptr as *mut c_void,
                collect,
                &mut cb_state as *mut _ as *mut c_void,
            );
            assert!(ok, "{}", error_string(rt));
            assert_eq!(cb_state.1.other, 0);
            assert_eq!(cb_state.1.c00 + cb_state.1.c11, 200);
            assert!(cb_state.1.c00 > 50 && cb_state.1.c11 > 50);
            noev_rt_free(rt);
        }
    }
}
