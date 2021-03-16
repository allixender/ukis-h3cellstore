use std::collections::HashMap;
use std::sync::Arc;

use h3ron::Index;
use log::warn;
use numpy::{IntoPyArray, PyArray1, PyReadonlyArray1};
use pyo3::{prelude::*, PyResult, Python};

use bamboo_h3_int::compacted_tables::TableSetQuery;
use bamboo_h3_int::{ColVec, COL_NAME_H3INDEX};

use crate::{
    inspect::TableSet as TableSetWrapper,
    pywrap::{check_index_valid, intresult_to_pyresult, Polygon},
    syncapi::{ClickhousePool, Query},
    window::SlidingH3Window,
};
use either::Either;
use either::Either::{Left, Right};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle as TaskJoinHandle;

#[pyclass]
pub struct ClickhouseConnection {
    pub(crate) clickhouse_pool: Arc<ClickhousePool>,
}

impl ClickhouseConnection {
    pub fn new(clickhouse_pool: Arc<ClickhousePool>) -> Self {
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
        crate::window::create_window(
            self.clickhouse_pool.clone(),
            window_polygon.inner.clone(),
            tableset.inner.clone(),
            target_h3_resolution,
            window_max_size,
            if let Some(s) = querystring_template {
                TableSetQuery::TemplatedSelect(s)
            } else {
                TableSetQuery::AutoGenerated
            },
            prefetch_querystring_template.map(TableSetQuery::TemplatedSelect),
        )
    }

    fn list_tablesets(&mut self) -> PyResult<HashMap<String, TableSetWrapper>> {
        Ok(self
            .clickhouse_pool
            .list_tablesets()?
            .drain()
            .map(|(k, v)| (k, TableSetWrapper { inner: v }))
            .collect())
    }

    fn query_fetch(&mut self, query_string: String) -> PyResult<ResultSet> {
        let awrs =
            AwaitableResultSet::new(self.clickhouse_pool.clone(), Query::Plain(query_string));
        Ok(awrs.into())
    }

    #[args(query_template = "None")]
    fn tableset_fetch(
        &mut self,
        tableset: &TableSetWrapper,
        h3indexes: PyReadonlyArray1<u64>,
        query_template: Option<String>,
    ) -> PyResult<ResultSet> {
        let h3indexes_vec = h3indexes.as_array().to_vec();
        let query_string = intresult_to_pyresult(
            tableset
                .inner
                .build_select_query(&h3indexes_vec, &query_template.into()),
        )?;

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
        let index = Index::from(h3index);
        check_index_valid(&index)?;

        let tablesetquery = match query_template {
            Some(qs) => TableSetQuery::TemplatedSelect(format!("{} limit 1", qs)),
            None => TableSetQuery::TemplatedSelect(format!(
                "select {} from <[table]> where {} in <[h3indexes]> limit 1",
                COL_NAME_H3INDEX, COL_NAME_H3INDEX,
            )),
        };
        let query_string = intresult_to_pyresult(
            tableset
                .inner
                .build_select_query(&[index.h3index()], &tablesetquery),
        )?;
        self.clickhouse_pool.query_returns_rows(query_string)
    }
}

pub(crate) struct AwaitableResultSet {
    pub clickhouse_pool: Arc<ClickhousePool>,
    pub handle: Option<TaskJoinHandle<PyResult<HashMap<String, ColVec>>>>,

    /// time the query started
    pub t_query_start: Instant,
}

impl AwaitableResultSet {
    pub fn new(clickhouse_pool: Arc<ClickhousePool>, query: Query) -> Self {
        let handle = Some(clickhouse_pool.spawn_query(query));
        Self {
            clickhouse_pool,
            handle,
            t_query_start: Instant::now(),
        }
    }

    pub fn wait_until_finished(&mut self) -> PyResult<(HashMap<String, ColVec>, Duration)> {
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
    pub(crate) column_data: Either<HashMap<String, ColVec>, Option<AwaitableResultSet>>,

    /// the duration the query took to finish
    /// Not measured for all queries
    query_duration: Option<Duration>,
}

impl ResultSet {
    pub(crate) fn await_column_data(&mut self) -> PyResult<()> {
        if let Either::Right(maybe_awaitable) = &mut self.column_data {
            if let Some(mut awaitable) = maybe_awaitable.take() {
                let (columns_hashmap, query_duration) = awaitable.wait_until_finished()?;
                self.column_data = Left(columns_hashmap);
                self.query_duration = Some(query_duration);
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
            column_data: Left(column_data),
            query_duration: None,
        }
    }
}

impl From<AwaitableResultSet> for ResultSet {
    fn from(awrs: AwaitableResultSet) -> Self {
        Self {
            h3indexes_queried: None,
            window_h3index: None,
            column_data: Right(Some(awrs)),
            query_duration: None,
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
    /// get the names and types of the columns in the resultset
    ///
    /// Calling this results in waiting until the results are available.
    fn get_column_types(&mut self) -> PyResult<HashMap<String, String>> {
        self.await_column_data()?;
        match &self.column_data {
            Either::Left(cd) => Ok(cd
                .iter()
                .map(|(name, data)| (name.clone(), data.type_name().to_string()))
                .collect()),
            Either::Right(_) => Ok(Default::default()),
        }
    }

    /// Calling this results in waiting until the results are available.
    pub fn is_empty(&mut self) -> PyResult<bool> {
        self.await_column_data()?;
        if let Left(cd) = &self.column_data {
            if cd.is_empty() {
                return Ok(true);
            }
            for (_, v) in cd.iter() {
                if !v.is_empty() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
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

pub(crate) fn validate_clickhouse_url(u: &str) -> PyResult<()> {
    let parsed_url = url::Url::parse(u)
        .map_err(|e| PyValueError::new_err(format!("Invalid Url given: {:?}", e)))?;

    let parameters: HashMap<_, _> = parsed_url
        .query_pairs()
        .map(|(name, value)| (name.to_lowercase(), value.to_string()))
        .collect();

    if parameters
        .get("compression")
        .cloned()
        .unwrap_or_else(|| "none".to_string())
        == *"none"
    {
        warn!("possible inefficient data transfer: consider setting a compression_method in the clickhouse connection parameters. 'lz4' is one option.")
    }
    if parameters.get("connection_timeout").is_none() {
        warn!("short connection_timeout: clickhouse connection parameters sets no connection_timeout, so it uses the very short default of 500ms")
    }

    Ok(())
}
