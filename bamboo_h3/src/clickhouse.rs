use std::collections::HashMap;
use std::convert::TryFrom;
use std::time::{Duration, Instant};

use either::Either;
use h3ron::{H3Cell, Index};
use numpy::{IntoPyArray, PyArray1, PyReadonlyArray1};
use pyo3::exceptions::PyRuntimeError;
use pyo3::{prelude::*, PyResult, Python};
use tokio::task::JoinHandle as TaskJoinHandle;

use bamboo_h3_core::clickhouse::compacted_tables::TableSetQuery;
use bamboo_h3_core::{ColVec, COL_NAME_H3INDEX};

use crate::columnset::ColumnSet;
use crate::schema::Schema;
use crate::{
    error::IntoPyResult,
    geo::Polygon,
    inspect::TableSet as TableSetWrapper,
    syncapi::{ClickhousePool, Query},
    window::SlidingH3Window,
};
use bamboo_h3_core::clickhouse::window::SlidingWindowOptions;

#[pyclass]
pub struct ClickhouseConnection {
    pub(crate) clickhouse_pool: ClickhousePool,
}

impl ClickhouseConnection {
    pub fn new(clickhouse_pool: ClickhousePool) -> Self {
        Self { clickhouse_pool }
    }
}

#[pymethods]
impl ClickhouseConnection {
    #[allow(clippy::too_many_arguments)]
    #[args(querystring_template = "None", prefetch_querystring_template = "None")]
    pub fn make_sliding_window(
        &self,
        window_polygon: &Polygon,
        tableset: &TableSetWrapper,
        target_h3_resolution: u8,
        window_max_size: u32,
        querystring_template: Option<String>,
        prefetch_querystring_template: Option<String>,
    ) -> PyResult<SlidingH3Window> {
        let opts = SlidingWindowOptions {
            window_polygon: window_polygon.inner.clone(),
            target_h3_resolution,
            window_max_size,
            tableset: tableset.inner.clone(),
            query: if let Some(s) = querystring_template {
                TableSetQuery::TemplatedSelect(s)
            } else {
                TableSetQuery::AutoGenerated
            },
            prefetch_query: prefetch_querystring_template.map(TableSetQuery::TemplatedSelect),
            concurrency: crate::env::window_num_concurrent_queries(),
            window_num_clickhouse_threads: crate::env::window_num_clickhouse_threads()
        };
        SlidingH3Window::create(self.clickhouse_pool.clone(), opts)
    }

    fn list_tablesets(&mut self) -> PyResult<HashMap<String, TableSetWrapper>> {
        Ok(self
            .clickhouse_pool
            .list_tablesets()?
            .drain()
            .map(|(k, v)| (k, TableSetWrapper { inner: v }))
            .collect())
    }

    fn drop_tableset(&mut self, tableset: TableSetWrapper) -> PyResult<()> {
        self.clickhouse_pool.drop_tableset(&tableset.inner)?;
        Ok(())
    }

    fn query_fetch(&mut self, query_string: String) -> ResultSet {
        AwaitableResultSet::new(self.clickhouse_pool.clone(), Query::Plain(query_string)).into()
    }

    #[args(query_template = "None")]
    fn tableset_fetch(
        &mut self,
        tableset: &TableSetWrapper,
        h3indexes: PyReadonlyArray1<u64>,
        query_template: Option<String>,
    ) -> PyResult<ResultSet> {
        let h3indexes_vec = h3indexes.as_array().to_vec();
        let query_string = tableset
            .inner
            .build_select_query(&h3indexes_vec, &query_template.into())
            .into_pyresult()?;

        let mut resultset: ResultSet = AwaitableResultSet::new(
            self.clickhouse_pool.clone(),
            Query::Uncompact(query_string, h3indexes_vec.iter().cloned().collect()),
        )
        .into();
        resultset.h3indexes_queried = Some(h3indexes_vec);
        Ok(resultset)
    }

    /// check if the tableset contains the h3index or any of its parents
    #[args(query_template = "None")]
    fn tableset_contains_h3index(
        &mut self,
        tableset: &TableSetWrapper,
        h3index: u64,
        query_template: Option<String>,
    ) -> PyResult<bool> {
        let index = H3Cell::try_from(h3index).into_pyresult()?;
        let tablesetquery = match query_template {
            Some(qs) => TableSetQuery::TemplatedSelect(format!("{} limit 1", qs)),
            None => TableSetQuery::TemplatedSelect(format!(
                "select {} from <[table]> where {} in <[h3indexes]> limit 1",
                COL_NAME_H3INDEX, COL_NAME_H3INDEX,
            )),
        };
        let query_string = tableset
            .inner
            .build_select_query(&[index.h3index()], &tablesetquery)
            .into_pyresult()?;
        self.clickhouse_pool.query_returns_rows(query_string)
    }

    fn create_schema(&mut self, schema: &Schema) -> PyResult<()> {
        self.clickhouse_pool.create_schema(schema)
    }

    fn save_columnset(&mut self, schema: &Schema, columnset: &ColumnSet) -> PyResult<()> {
        self.clickhouse_pool.save_columnset(schema, columnset)
    }
}

pub(crate) struct AwaitableResultSet {
    pub clickhouse_pool: ClickhousePool,
    pub handle: Option<TaskJoinHandle<PyResult<ColumnSet>>>,

    /// time the query started
    pub t_query_start: Instant,
}

impl AwaitableResultSet {
    pub fn new(clickhouse_pool: ClickhousePool, query: Query) -> Self {
        let handle = Some(clickhouse_pool.spawn_query(query));
        Self {
            clickhouse_pool,
            handle,
            t_query_start: Instant::now(),
        }
    }

    pub fn wait_until_finished(&mut self) -> PyResult<(ColumnSet, Duration)> {
        if let Some(handle) = self.handle.take() {
            let resultmap = self.clickhouse_pool.await_query(handle)?;
            Ok((resultmap, self.t_query_start.elapsed()))
        } else {
            Err(PyRuntimeError::new_err(
                "resultset can only be awaited once".to_string(),
            ))
        }
    }
}

#[pyclass]
pub struct ResultSet {
    pub(crate) h3indexes_queried: Option<Vec<u64>>,
    pub(crate) window_h3index: Option<u64>,
    pub(crate) column_data: Either<Option<ColumnSet>, Option<AwaitableResultSet>>,

    /// the duration the query took to finish
    /// Not measured for all queries
    pub(crate) query_duration: Option<Duration>,
}

impl ResultSet {
    pub(crate) fn await_column_data(&mut self) -> PyResult<()> {
        if let Either::Right(maybe_awaitable) = &mut self.column_data {
            if let Some(mut awaitable) = maybe_awaitable.take() {
                let (columnset, query_duration) = awaitable.wait_until_finished()?;
                self.column_data = Either::Left(Some(columnset));
                self.query_duration = Some(query_duration);
            } else {
                return Err(PyRuntimeError::new_err("Got None for AwaitableResultSet"));
            }
        }
        Ok(())
    }
}

impl From<HashMap<String, ColVec>> for ResultSet {
    fn from(column_data: HashMap<String, ColVec>) -> Self {
        Self {
            h3indexes_queried: None,
            window_h3index: None,
            column_data: Either::Left(Some(column_data.into())),
            query_duration: None,
        }
    }
}

impl From<AwaitableResultSet> for ResultSet {
    fn from(awrs: AwaitableResultSet) -> Self {
        Self {
            h3indexes_queried: None,
            window_h3index: None,
            column_data: Either::Right(Some(awrs)),
            query_duration: None,
        }
    }
}

impl From<bamboo_h3_core::clickhouse::QueryOutput<bamboo_h3_core::ColumnSet>> for ResultSet {
    fn from(qo: bamboo_h3_core::clickhouse::QueryOutput<bamboo_h3_core::ColumnSet>) -> Self {
        Self {
            h3indexes_queried: qo.h3indexes_queried,
            window_h3index: qo.window_h3index,
            column_data: Either::Left(Some(qo.data.into())),
            query_duration: qo.query_duration
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
    fn get_window_index(&self) -> Option<u64> {
        self.window_h3index
    }

    /// get the contents fetched in resultset
    ///
    /// This can be done only once as the ownership get passed to python.
    ///
    /// Calling this results in waiting until the results are available.
    #[allow(clippy::wrong_self_convention)]
    fn to_columnset(&mut self) -> PyResult<Option<ColumnSet>> {
        self.await_column_data()?;
        match self.column_data.as_mut() {
            Either::Left(cd) => Ok(cd.take()),
            Either::Right(_) => Ok(None),
        }
    }

    #[getter]
    /// Calling this results in waiting until the results are available.
    fn get_empty(&mut self) -> PyResult<bool> {
        self.await_column_data()?;
        match &self.column_data {
            Either::Left(cd) => Ok(cd.as_ref().map_or_else(|| true, |cs| cs.inner.is_empty())),
            Either::Right(_) => Err(PyRuntimeError::new_err("awaiting failed")),
        }
    }

    #[getter]
    /// the number of seconds the query took to execute
    ///
    /// Only measured for async queries, so this may be None.
    /// Calling this results in waiting until the results are available.
    pub fn get_query_duration_secs(&mut self) -> PyResult<Option<f64>> {
        self.await_column_data()?;
        Ok(self.query_duration.map(|d| d.as_millis() as f64 / 1000.0))
    }
}
