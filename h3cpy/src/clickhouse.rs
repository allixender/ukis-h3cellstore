use std::collections::{HashMap, HashSet};

use geo::algorithm::intersects::Intersects;
use h3ron::{Index, ToPolygon};
use numpy::{IntoPyArray, PyArray1, PyReadonlyArray1};
use pyo3::{exceptions::PyRuntimeError, prelude::*, PyResult, Python};
use tokio::runtime::Runtime;

use h3cpy_int::compacted_tables::TableSetQuery;
use h3cpy_int::{
    clickhouse::query::{
        list_tablesets, query_all, query_all_with_uncompacting, query_returns_rows,
    },
    clickhouse_rs::{errors::Error as ChError, errors::Result as ChResult, ClientHandle, Pool},
    ColVec,
};

use crate::{
    inspect::TableSet as TableSetWrapper,
    pywrap::{check_index_valid, intresult_to_pyresult, Polygon},
    window::{create_window, SlidingH3Window},
};

fn ch_to_pyerr(ch_err: ChError) -> PyErr {
    PyRuntimeError::new_err(format!("clickhouse error: {:?}", ch_err))
}

fn ch_to_pyresult<T>(res: ChResult<T>) -> PyResult<T> {
    match res {
        Ok(v) => Ok(v),
        Err(e) => Err(ch_to_pyerr(e)),
    }
}

pub(crate) struct RuntimedPool {
    pub(crate) pool: Pool,
    pub(crate) rt: Runtime,
}

impl RuntimedPool {
    pub fn create(db_url: &str) -> PyResult<RuntimedPool> {
        let rt = match Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                return Err(PyRuntimeError::new_err(format!(
                    "could not create tokio rt: {:?}",
                    e
                )))
            }
        };
        Ok(Self {
            pool: Pool::new(db_url),
            rt,
        })
    }

    pub fn get_client(&mut self) -> PyResult<ClientHandle> {
        let p = &self.pool;
        ch_to_pyresult(self.rt.block_on(async { p.get_handle().await }))
    }
}

#[pyclass]
pub struct ClickhouseConnection {
    pub(crate) rp: RuntimedPool,
}

#[pymethods]
impl ClickhouseConnection {
    #[args(querystring_template = "None")]
    pub fn make_sliding_window(
        &self,
        window_polygon: &Polygon,
        tableset: &TableSetWrapper,
        target_h3_resolution: u8,
        window_max_size: u32,
        querystring_template: Option<String>,
    ) -> PyResult<SlidingH3Window> {
        create_window(
            window_polygon.inner.clone(),
            &tableset.inner,
            target_h3_resolution,
            window_max_size,
            querystring_template.into(),
        )
    }

    fn list_tablesets(&mut self) -> PyResult<HashMap<String, TableSetWrapper>> {
        let client = self.rp.get_client()?;
        let mut ts = ch_to_pyresult(self.rp.rt.block_on(async { list_tablesets(client).await }))?;
        Ok(ts
            .drain()
            .map(|(k, v)| (k, TableSetWrapper { inner: v }))
            .collect())
    }

    fn fetch_query(&mut self, query_string: String) -> PyResult<ResultSet> {
        let client = self.rp.get_client()?;
        let column_data = ch_to_pyresult(
            self.rp
                .rt
                .block_on(async { query_all(client, query_string).await }),
        )?;
        Ok(column_data.into())
    }

    #[args(querystring_template = "None")]
    fn fetch_tableset(
        &mut self,
        tableset: &TableSetWrapper,
        h3indexes: PyReadonlyArray1<u64>,
        querystring_template: Option<String>,
    ) -> PyResult<ResultSet> {
        let h3indexes_vec = h3indexes.as_array().to_vec();
        let query_string = intresult_to_pyresult(
            tableset
                .inner
                .build_select_query(&h3indexes_vec, &querystring_template.into()),
        )?;

        let client = self.rp.get_client()?;
        let column_data = ch_to_pyresult(self.rp.rt.block_on(async {
            let h3index_set: HashSet<_> = h3indexes_vec.iter().cloned().collect();
            query_all_with_uncompacting(client, query_string, h3index_set).await
        }))?;
        let mut resultset: ResultSet = column_data.into();
        resultset.h3indexes_queried = Some(h3indexes_vec);
        Ok(resultset)
    }

    /// check if the tableset contains the h3index or any of its parents
    #[args(querystring_template = "None")]
    fn has_data(
        &mut self,
        tableset: &TableSetWrapper,
        h3index: u64,
        querystring_template: Option<String>,
    ) -> PyResult<bool> {
        let index = Index::from(h3index);
        check_index_valid(&index)?;

        let tablesetquery = match querystring_template {
            Some(qs) => TableSetQuery::TemplatedSelect(format!("{} limit 1", qs)),
            None => TableSetQuery::TemplatedSelect(
                "select h3index from <[table]> where h3index in <[h3indexes]> limit 1".to_string(),
            ),
        };
        let query_string = intresult_to_pyresult(
            tableset
                .inner
                .build_select_query(&[index.h3index()], &tablesetquery),
        )?;

        let client = self.rp.get_client()?;
        ch_to_pyresult(
            self.rp
                .rt
                .block_on(async { query_returns_rows(client, query_string).await }),
        )
    }

    pub fn fetch_next_window(
        &mut self,
        sliding_h3_window: &mut SlidingH3Window,
        tableset: &TableSetWrapper,
    ) -> PyResult<Option<ResultSet>> {
        while let Some(window_h3index) = sliding_h3_window.next_window() {
            // check if the window index contains any data on coarse resolution, when not,
            // then there is no need to load anything
            if !self.has_data(
                tableset,
                window_h3index,
                sliding_h3_window.query.clone().into(),
            )? {
                log::info!("window without any database contents skipped");
                continue;
            }

            let child_indexes: Vec<_> = Index::from(window_h3index)
                .get_children(sliding_h3_window.target_h3_resolution)
                .drain(..)
                // remove children located outside the window_polygon. It is probably is not worth the effort,
                // but it allows to relocate some load to the client.
                .filter(|ci| {
                    let p = ci.to_polygon();
                    sliding_h3_window.window_polygon.intersects(&p)
                })
                .map(|i| i.h3index())
                .collect();

            if child_indexes.is_empty() {
                log::info!("window without intersecting h3indexes skipped");
                continue;
            }

            let query_string = intresult_to_pyresult(
                tableset
                    .inner
                    .build_select_query(&child_indexes, &sliding_h3_window.query),
            )?;
            let client = self.rp.get_client()?;
            let mut resultset: ResultSet = ch_to_pyresult(self.rp.rt.block_on(async {
                let h3index_set: HashSet<_> = child_indexes.iter().cloned().collect();
                query_all_with_uncompacting(client, query_string, h3index_set).await
            }))?
            .into();
            resultset.h3indexes_queried = Some(child_indexes);
            resultset.window_h3index = Some(window_h3index);

            return Ok(Some(resultset));
        }
        Ok(None)
    }
}

#[pyclass]
pub struct ResultSet {
    h3indexes_queried: Option<Vec<u64>>,
    window_h3index: Option<u64>,
    pub(crate) column_data: HashMap<String, ColVec>,
}

#[pymethods]
impl ResultSet {
    pub fn is_empty(&self) -> bool {
        if self.column_data.is_empty() {
            return true;
        }
        for (_, v) in self.column_data.iter() {
            if !v.is_empty() {
                return false;
            }
        }
        true
    }
}

impl From<HashMap<String, ColVec>> for ResultSet {
    fn from(column_data: HashMap<String, ColVec>) -> Self {
        Self {
            h3indexes_queried: None,
            window_h3index: None,
            column_data,
        }
    }
}

#[pymethods]
impl ResultSet {
    /// get the number of h3indexes which where used in the query
    #[getter]
    fn get_num_h3indexes_queried(&self) -> Option<usize> {
        match &self.h3indexes_queried {
            Some(a) => Some(a.len()),
            None => None,
        }
    }

    /// get the h3indexes which where used in the query as a numpy array
    #[getter]
    fn get_h3indexes_queried(&self, py: Python) -> Py<PyArray1<u64>> {
        let h3vec = match &self.h3indexes_queried {
            Some(a) => a.clone(),
            None => vec![],
        };
        h3vec.into_pyarray(py).to_owned()
    }

    /// get the h3index of the window in case this resultset was fetched in a
    /// sliding window
    #[getter]
    fn get_window_index(&self) -> PyResult<Option<u64>> {
        Ok(self.window_h3index)
    }

    #[getter]
    fn get_column_types(&self) -> PyResult<HashMap<String, String>> {
        Ok(self
            .column_data
            .iter()
            .map(|(name, data)| (name.clone(), data.type_name().to_string()))
            .collect())
    }
}
