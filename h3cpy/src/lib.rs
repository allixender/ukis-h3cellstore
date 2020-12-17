mod window;
mod inspect;

use pyo3::prelude::*;
use pyo3::wrap_pyfunction;
use crate::{
    inspect::CompactedTable
};

/// Formats the sum of two numbers as string.
#[pyfunction]
fn sum_as_string(a: usize, b: usize) -> PyResult<String> {
    Ok((a + b).to_string())
}

/// A Python module implemented in Rust.
#[pymodule]
fn h3cpy(py: Python, m: &PyModule) -> PyResult<()> {
    m.add("CompactedTable", py.get_type::<CompactedTable>())?;
    m.add_function(wrap_pyfunction!(sum_as_string, m)?)?;

    Ok(())
}