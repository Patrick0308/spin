use anyhow::{anyhow, Result};
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use spin_core::{async_trait, wasmtime::component::Resource, HostComponent};
use spin_world::v1::{
    postgres as v1,
    rdbms_types::{Column, DbDataType, DbValue, ParameterValue, RowSet},
};
use spin_world::v2::postgres::{self as v2, Connection};
use tokio_postgres::{
    config::SslMode,
    types::{ToSql, Type},
    Client, NoTls, Row, Socket,
};

/// A simple implementation to support outbound pg connection
#[derive(Default)]
pub struct OutboundPg {
    pub connections: table::Table<Client>,
}

impl OutboundPg {
    async fn get_client(&mut self, connection: Resource<Connection>) -> Result<&Client, v2::Error> {
        self.connections
            .get(connection.rep())
            .ok_or_else(|| v2::Error::ConnectionFailed("no connection found".into()))
    }
}

impl HostComponent for OutboundPg {
    type Data = Self;

    fn add_to_linker<T: Send>(
        linker: &mut spin_core::Linker<T>,
        get: impl Fn(&mut spin_core::Data<T>) -> &mut Self::Data + Send + Sync + Copy + 'static,
    ) -> anyhow::Result<()> {
        v1::add_to_linker(linker, get)?;
        v2::add_to_linker(linker, get)
    }

    fn build_data(&self) -> Self::Data {
        Default::default()
    }
}

#[async_trait]
impl v2::Host for OutboundPg {}

#[async_trait]
impl v2::HostConnection for OutboundPg {
    async fn open(&mut self, address: String) -> Result<Result<Resource<Connection>, v2::Error>> {
        Ok(async {
            self.connections
                .push(
                    build_client(&address)
                        .await
                        .map_err(|e| v2::Error::ConnectionFailed(format!("{e:?}")))?,
                )
                .map_err(|_| v2::Error::ConnectionFailed("too many connections".into()))
                .map(Resource::new_own)
        }
        .await)
    }

    async fn execute(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<u64, v2::Error>> {
        Ok(async {
            let params: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(to_sql_parameter)
                .collect::<anyhow::Result<Vec<_>>>()
                .map_err(|e| v2::Error::ValueConversionFailed(format!("{:?}", e)))?;

            let nrow = self
                .get_client(connection)
                .await?
                .execute(&statement, params.as_slice())
                .await
                .map_err(|e| v2::Error::QueryFailed(format!("{:?}", e)))?;

            Ok(nrow)
        }
        .await)
    }

    async fn query(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<RowSet, v2::Error>> {
        Ok(async {
            let params: Vec<&(dyn ToSql + Sync)> = params
                .iter()
                .map(to_sql_parameter)
                .collect::<anyhow::Result<Vec<_>>>()
                .map_err(|e| v2::Error::BadParameter(format!("{:?}", e)))?;

            let results = self
                .get_client(connection)
                .await?
                .query(&statement, params.as_slice())
                .await
                .map_err(|e| v2::Error::QueryFailed(format!("{:?}", e)))?;

            if results.is_empty() {
                return Ok(RowSet {
                    columns: vec![],
                    rows: vec![],
                });
            }

            let columns = infer_columns(&results[0]);
            let rows = results
                .iter()
                .map(convert_row)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| v2::Error::QueryFailed(format!("{:?}", e)))?;

            Ok(RowSet { columns, rows })
        }
        .await)
    }

    fn drop(&mut self, connection: Resource<Connection>) -> anyhow::Result<()> {
        self.connections.remove(connection.rep());
        Ok(())
    }
}

fn to_sql_parameter(value: &ParameterValue) -> anyhow::Result<&(dyn ToSql + Sync)> {
    match value {
        ParameterValue::Boolean(v) => Ok(v),
        ParameterValue::Int32(v) => Ok(v),
        ParameterValue::Int64(v) => Ok(v),
        ParameterValue::Int8(v) => Ok(v),
        ParameterValue::Int16(v) => Ok(v),
        ParameterValue::Floating32(v) => Ok(v),
        ParameterValue::Floating64(v) => Ok(v),
        ParameterValue::Uint8(_)
        | ParameterValue::Uint16(_)
        | ParameterValue::Uint32(_)
        | ParameterValue::Uint64(_) => Err(anyhow!("Postgres does not support unsigned integers")),
        ParameterValue::Str(v) => Ok(v),
        ParameterValue::Binary(v) => Ok(v),
        ParameterValue::DbNull => Ok(&PgNull),
    }
}

fn infer_columns(row: &Row) -> Vec<Column> {
    let mut result = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        result.push(infer_column(row, index));
    }
    result
}

fn infer_column(row: &Row, index: usize) -> Column {
    let column = &row.columns()[index];
    let name = column.name().to_owned();
    let data_type = convert_data_type(column.type_());
    Column { name, data_type }
}

fn convert_data_type(pg_type: &Type) -> DbDataType {
    match *pg_type {
        Type::BOOL => DbDataType::Boolean,
        Type::BYTEA => DbDataType::Binary,
        Type::FLOAT4 => DbDataType::Floating32,
        Type::FLOAT8 => DbDataType::Floating64,
        Type::INT2 => DbDataType::Int16,
        Type::INT4 => DbDataType::Int32,
        Type::INT8 => DbDataType::Int64,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR => DbDataType::Str,
        _ => {
            tracing::debug!("Couldn't convert Postgres type {} to WIT", pg_type.name(),);
            DbDataType::Other
        }
    }
}

fn convert_row(row: &Row) -> Result<Vec<DbValue>, tokio_postgres::Error> {
    let mut result = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        result.push(convert_entry(row, index)?);
    }
    Ok(result)
}

fn convert_entry(row: &Row, index: usize) -> Result<DbValue, tokio_postgres::Error> {
    let column = &row.columns()[index];
    let value = match column.type_() {
        &Type::BOOL => {
            let value: Option<bool> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Boolean(v),
                None => DbValue::DbNull,
            }
        }
        &Type::BYTEA => {
            let value: Option<Vec<u8>> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Binary(v),
                None => DbValue::DbNull,
            }
        }
        &Type::FLOAT4 => {
            let value: Option<f32> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Floating32(v),
                None => DbValue::DbNull,
            }
        }
        &Type::FLOAT8 => {
            let value: Option<f64> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Floating64(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT2 => {
            let value: Option<i16> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int16(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT4 => {
            let value: Option<i32> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int32(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT8 => {
            let value: Option<i64> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int64(v),
                None => DbValue::DbNull,
            }
        }
        &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR => {
            let value: Option<String> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Str(v),
                None => DbValue::DbNull,
            }
        }
        t => {
            tracing::debug!(
                "Couldn't convert Postgres type {} in column {}",
                t.name(),
                column.name()
            );
            DbValue::Unsupported
        }
    };
    Ok(value)
}

async fn build_client(address: &str) -> anyhow::Result<Client> {
    let config = address.parse::<tokio_postgres::Config>()?;

    tracing::debug!("Build new connection: {}", address);

    if config.get_ssl_mode() == SslMode::Disable {
        connect(config).await
    } else {
        connect_tls(config).await
    }
}

async fn connect(config: tokio_postgres::Config) -> anyhow::Result<Client> {
    let (client, connection) = config.connect(NoTls).await?;

    spawn(connection);

    Ok(client)
}

async fn connect_tls(config: tokio_postgres::Config) -> anyhow::Result<Client> {
    let builder = TlsConnector::builder();
    let connector = MakeTlsConnector::new(builder.build()?);
    let (client, connection) = config.connect(connector).await?;

    spawn(connection);

    Ok(client)
}

fn spawn<T>(connection: tokio_postgres::Connection<Socket, T>)
where
    T: tokio_postgres::tls::TlsStream + std::marker::Unpin + std::marker::Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!("Postgres connection error: {}", e);
        }
    });
}

/// Although the Postgres crate converts Rust Option::None to Postgres NULL,
/// it enforces the type of the Option as it does so. (For example, trying to
/// pass an Option::<i32>::None to a VARCHAR column fails conversion.) As we
/// do not know expected column types, we instead use a "neutral" custom type
/// which allows conversion to any type but always tells the Postgres crate to
/// treat it as a SQL NULL.
struct PgNull;

impl ToSql for PgNull {
    fn to_sql(
        &self,
        _ty: &Type,
        _out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    where
        Self: Sized,
    {
        Ok(tokio_postgres::types::IsNull::Yes)
    }

    fn accepts(_ty: &Type) -> bool
    where
        Self: Sized,
    {
        true
    }

    fn to_sql_checked(
        &self,
        _ty: &Type,
        _out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        Ok(tokio_postgres::types::IsNull::Yes)
    }
}

impl std::fmt::Debug for PgNull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NULL").finish()
    }
}

/// Delegate a function call to the v2::HostConnection implementation
macro_rules! delegate {
    ($self:ident.$name:ident($address:expr, $($arg:expr),*)) => {{
        let connection = match <Self as v2::HostConnection>::open($self, $address).await? {
            Ok(c) => c,
            Err(e) => return Ok(Err(to_legacy_error(e))),
        };
        Ok(<Self as v2::HostConnection>::$name($self, connection, $($arg),*)
            .await?
            .map_err(|e| to_legacy_error(e)))
    }};
}

#[async_trait]
impl v1::Host for OutboundPg {
    async fn execute(
        &mut self,
        address: String,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<u64, v1::PgError>> {
        delegate!(self.execute(address, statement, params))
    }

    async fn query(
        &mut self,
        address: String,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<RowSet, v1::PgError>> {
        delegate!(self.query(address, statement, params))
    }
}

fn to_legacy_error(error: v2::Error) -> v1::PgError {
    match error {
        v2::Error::ConnectionFailed(e) => v1::PgError::ConnectionFailed(e),
        v2::Error::BadParameter(e) => v1::PgError::BadParameter(e),
        v2::Error::QueryFailed(e) => v1::PgError::QueryFailed(e),
        v2::Error::ValueConversionFailed(e) => v1::PgError::ValueConversionFailed(e),
        v2::Error::Other(e) => v1::PgError::OtherError(e),
    }
}
