use super::schema;
use super::super::{Datastore, EdgeDirection, EdgeQuery, Transaction, VertexQuery};
use super::util::CTEQueryBuilder;
use chrono::DateTime;
use chrono::offset::Utc;
use errors::{Error, Result};
use models;
use num_cpus;
use postgres;
use postgres::types::ToSql;
use r2d2::{Pool, PooledConnection};
use r2d2_postgres::{PostgresConnectionManager, TlsMode};
use serde_json::Value as JsonValue;
use std::cmp::min;
use std::i64;
use std::mem;
use util::generate_uuid_v1;
use uuid::Uuid;

/// A datastore that is backed by a postgres database.
#[derive(Clone, Debug)]
pub struct PostgresDatastore {
    pool: Pool<PostgresConnectionManager>,
}

impl PostgresDatastore {
    /// Creates a new postgres-backed datastore.
    ///
    /// # Arguments
    /// * `pool_size` - The maximum number of connections to maintain to
    ///   postgres. If `None`, it defaults to twice the number of CPUs.
    /// * `connetion_string` - The postgres database connection string.
    pub fn new(pool_size: Option<u32>, connection_string: String) -> Result<PostgresDatastore> {
        let unwrapped_pool_size: u32 = match pool_size {
            Some(val) => val,
            None => min(num_cpus::get() as u32, 128u32),
        };

        let manager = PostgresConnectionManager::new(&*connection_string, TlsMode::None)?;
        let pool = Pool::builder()
            .max_size(unwrapped_pool_size)
            .build(manager)?;

        Ok(PostgresDatastore { pool: pool })
    }

    /// Creates a new postgres-backed datastore.
    ///
    /// # Arguments
    /// * `connetion_string` - The postgres database connection string.
    pub fn create_schema(connection_string: String) -> Result<()> {
        let conn = postgres::Connection::connect(connection_string, postgres::TlsMode::None)
            .map_err(|err| Error::with_chain(err, "Could not connect to the postgres database"))?;

        for statement in schema::SCHEMA.split(";") {
            conn.execute(statement, &vec![])?;
        }

        Ok(())
    }
}

impl Datastore<PostgresTransaction> for PostgresDatastore {
    fn transaction(&self) -> Result<PostgresTransaction> {
        let conn = self.pool.get()?;
        let trans = PostgresTransaction::new(conn)?;
        Ok(trans)
    }
}

/// A postgres-backed datastore transaction.
#[derive(Debug)]
pub struct PostgresTransaction {
    trans: postgres::transaction::Transaction<'static>,
    conn: Box<PooledConnection<PostgresConnectionManager>>,
}

impl PostgresTransaction {
    fn new(conn: PooledConnection<PostgresConnectionManager>) -> Result<Self> {
        let conn = Box::new(conn);

        let trans: postgres::transaction::Transaction<'static> = unsafe {
            mem::transmute(conn.transaction()
                .map_err(|err| Error::with_chain(err, "Could not create transaction"))?)
        };

        trans.set_commit();

        Ok(PostgresTransaction {
            conn: conn,
            trans: trans,
        })
    }

    fn vertex_query_to_sql(&self, q: &VertexQuery, sql_query_builder: &mut CTEQueryBuilder) {
        match q {
            &VertexQuery::All {
                ref start_id,
                ref limit,
            } => match start_id {
                &Some(start_id) => {
                    let query_template = "SELECT id, type FROM %t WHERE id > %p ORDER BY id LIMIT %p";
                    let params: Vec<Box<ToSql>> = vec![Box::new(start_id), Box::new(*limit as i64)];
                    sql_query_builder.push(query_template, "vertices", params);
                }
                &None => {
                    let query_template = "SELECT id, type FROM %t ORDER BY id LIMIT %p";
                    let params: Vec<Box<ToSql>> = vec![Box::new(*limit as i64)];
                    sql_query_builder.push(query_template, "vertices", params);
                }
            },
            &VertexQuery::Vertices { ref ids } => {
                let mut params_template_builder = vec![];
                let mut params: Vec<Box<ToSql>> = vec![];

                for id in ids {
                    params_template_builder.push("%p");
                    params.push(Box::new(*id));
                }

                let query_template = format!(
                    "SELECT id, type FROM %t WHERE id IN ({}) ORDER BY id",
                    params_template_builder.join(", ")
                );
                sql_query_builder.push(&query_template[..], "vertices", params);
            }
            &VertexQuery::Pipe {
                ref edge_query,
                ref converter,
                ref limit,
            } => {
                self.edge_query_to_sql(edge_query, sql_query_builder);
                let params: Vec<Box<ToSql>> = vec![Box::new(*limit as i64)];

                let query_template = match converter {
                    &EdgeDirection::Outbound => {
                        "SELECT id, type FROM vertices WHERE id IN (SELECT outbound_id FROM %t) ORDER BY id LIMIT %p"
                    }
                    &EdgeDirection::Inbound => {
                        "SELECT id, type FROM vertices WHERE id IN (SELECT inbound_id FROM %t) ORDER BY id LIMIT %p"
                    }
                };

                sql_query_builder.push(query_template, "", params);
            }
        }
    }

    fn edge_query_to_sql(&self, q: &EdgeQuery, sql_query_builder: &mut CTEQueryBuilder) {
        match q {
            &EdgeQuery::Edges { ref keys } => {
                let mut params_template_builder = vec![];
                let mut params: Vec<Box<ToSql>> = vec![];

                for key in keys {
                    params_template_builder.push("(%p, %p, %p)");
                    params.push(Box::new(key.outbound_id));
                    params.push(Box::new(key.t.0.to_string()));
                    params.push(Box::new(key.inbound_id));
                }

                let query_template = format!(
                    "SELECT id, outbound_id, type, inbound_id, update_timestamp FROM %t WHERE (outbound_id, type, inbound_id) IN ({})",
                    params_template_builder.join(", ")
                );
                sql_query_builder.push(&query_template[..], "edges", params);
            }
            &EdgeQuery::Pipe {
                ref vertex_query,
                converter,
                ref type_filter,
                high_filter,
                low_filter,
                limit,
            } => {
                self.vertex_query_to_sql(&*vertex_query, sql_query_builder);

                let mut where_clause_template_builder = vec![];
                let mut params: Vec<Box<ToSql>> = vec![];

                if let &Some(ref type_filter) = type_filter {
                    where_clause_template_builder.push("type = %p");
                    params.push(Box::new(type_filter.0.to_string()));
                }

                if let Some(high_filter) = high_filter {
                    where_clause_template_builder.push("update_timestamp <= %p");
                    params.push(Box::new(high_filter));
                }

                if let Some(low_filter) = low_filter {
                    where_clause_template_builder.push("update_timestamp >= %p");
                    params.push(Box::new(low_filter));
                }

                params.push(Box::new(limit as i64));
                let where_clause = where_clause_template_builder.join(" AND ");

                let query_template = match (converter, where_clause.len()) {
                    (EdgeDirection::Outbound, 0) => {
                        "SELECT id, outbound_id, type, inbound_id, update_timestamp FROM edges WHERE outbound_id IN (SELECT id FROM %t) ORDER BY update_timestamp DESC LIMIT %p".to_string()
                    }
                    (EdgeDirection::Outbound, _) => {
                        format!(
                            "SELECT id, outbound_id, type, inbound_id, update_timestamp FROM edges WHERE outbound_id IN (SELECT id FROM %t) AND {} ORDER BY update_timestamp DESC LIMIT %p",
                            where_clause
                        )
                    }
                    (EdgeDirection::Inbound, 0) => {
                        "SELECT id, outbound_id, type, inbound_id, update_timestamp FROM edges WHERE inbound_id IN (SELECT id FROM %t) ORDER BY update_timestamp DESC LIMIT %p".to_string()
                    }
                    (EdgeDirection::Inbound, _) => {
                        format!(
                            "SELECT id, outbound_id, type, inbound_id, update_timestamp FROM edges WHERE inbound_id IN (SELECT id FROM %t) AND {} ORDER BY update_timestamp DESC LIMIT %p",
                            where_clause
                        )
                    }
                };

                sql_query_builder.push(&query_template[..], "", params);
            }
        }
    }
}

impl Transaction for PostgresTransaction {
    fn create_vertex(&self, vertex: &models::Vertex) -> Result<bool> {
        // Because this command could fail, we need to set a savepoint to roll
        // back to, rather than spoiling the entire transaction
        let trans = self.trans.savepoint("create_vertex")?;

        let result = self.trans.execute(
            "INSERT INTO vertices (id, type) VALUES ($1, $2)",
            &[&vertex.id, &vertex.t.0],
        );

        if result.is_err() {
            trans.set_rollback();
            Ok(false)
        } else {
            trans.set_commit();
            Ok(true)
        }
    }

    fn get_vertices(&self, q: &VertexQuery) -> Result<Vec<models::Vertex>> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.vertex_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload("SELECT id, type FROM %t", vec![]);
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();

        let results = self.trans.query(&query[..], &params_refs[..])?;
        let mut vertices: Vec<models::Vertex> = Vec::new();

        for row in &results {
            let id: Uuid = row.get(0);
            let t_str: String = row.get(1);
            let v = models::Vertex::with_id(id, models::Type::new(t_str).unwrap());
            vertices.push(v);
        }

        Ok(vertices)
    }

    fn delete_vertices(&self, q: &VertexQuery) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.vertex_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "DELETE FROM vertices WHERE id IN (SELECT id FROM %t)",
            vec![],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }

    fn get_vertex_count(&self) -> Result<u64> {
        let results = self.trans.query("SELECT COUNT(*) FROM vertices", &[])?;

        for row in &results {
            let count: i64 = row.get(0);
            return Ok(count as u64);
        }

        unreachable!();
    }

    fn create_edge(&self, key: &models::EdgeKey) -> Result<bool> {
        let id = generate_uuid_v1();

        // Because this command could fail, we need to set a savepoint to roll
        // back to, rather than spoiling the entire transaction
        let trans = self.trans.savepoint("set_edge")?;

        let results = trans.query(
            "
            INSERT INTO edges (id, outbound_id, type, inbound_id, update_timestamp)
            VALUES ($1, $2, $3, $4, CLOCK_TIMESTAMP())
            ON CONFLICT ON CONSTRAINT edges_outbound_id_type_inbound_id_ukey
            DO UPDATE SET update_timestamp=CLOCK_TIMESTAMP()
        ",
            &[&id, &key.outbound_id, &key.t.0, &key.inbound_id],
        );

        if results.is_err() {
            trans.set_rollback();
            Ok(false)
        } else {
            trans.set_commit();
            Ok(true)
        }
    }

    fn get_edges(&self, q: &EdgeQuery) -> Result<Vec<models::Edge>> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.edge_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "SELECT outbound_id, type, inbound_id, update_timestamp FROM %t",
            vec![],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();

        let results = self.trans.query(&query[..], &params_refs[..])?;
        let mut edges: Vec<models::Edge> = Vec::new();

        for row in &results {
            let outbound_id: Uuid = row.get(0);
            let t_str: String = row.get(1);
            let inbound_id: Uuid = row.get(2);
            let update_datetime: DateTime<Utc> = row.get(3);
            let t = models::Type::new(t_str).unwrap();
            let key = models::EdgeKey::new(outbound_id, t, inbound_id);
            let edge = models::Edge::new(key, update_datetime);
            edges.push(edge);
        }

        Ok(edges)
    }

    fn delete_edges(&self, q: &EdgeQuery) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.edge_query_to_sql(q, &mut sql_query_builder);
        let (query, params) =
            sql_query_builder.into_query_payload("DELETE FROM edges WHERE id IN (SELECT id FROM %t)", vec![]);
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }

    fn get_edge_count(
        &self,
        id: Uuid,
        type_filter: Option<&models::Type>,
        direction: models::EdgeDirection,
    ) -> Result<u64> {
        let results = match (direction, type_filter) {
            (models::EdgeDirection::Outbound, Some(t)) => self.trans.query(
                "SELECT COUNT(*) FROM edges WHERE outbound_id=$1 AND type=$2",
                &[&id, &t.0],
            ),
            (models::EdgeDirection::Outbound, None) => self.trans
                .query("SELECT COUNT(*) FROM edges WHERE outbound_id=$1", &[&id]),
            (models::EdgeDirection::Inbound, Some(t)) => self.trans.query(
                "SELECT COUNT(*) FROM edges WHERE inbound_id=$1 AND type=$2",
                &[&id, &t.0],
            ),
            (models::EdgeDirection::Inbound, None) => self.trans
                .query("SELECT COUNT(*) FROM edges WHERE inbound_id=$1", &[&id]),
        }?;

        for row in &results {
            let count: i64 = row.get(0);
            return Ok(count as u64);
        }

        unreachable!();
    }

    fn get_vertex_metadata(&self, q: &VertexQuery, name: &str) -> Result<Vec<models::VertexMetadata>> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.vertex_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "SELECT owner_id, value FROM vertex_metadata WHERE owner_id IN (SELECT id FROM %t) AND name=%p",
            vec![Box::new(name.to_string())],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        let results = self.trans.query(&query[..], &params_refs[..])?;
        let mut metadata = Vec::new();

        for row in &results {
            let id: Uuid = row.get(0);
            let value: JsonValue = row.get(1);
            metadata.push(models::VertexMetadata::new(id, value));
        }

        Ok(metadata)
    }

    fn set_vertex_metadata(&self, q: &VertexQuery, name: &str, value: &JsonValue) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.vertex_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "
            INSERT INTO vertex_metadata (owner_id, name, value)
            SELECT id, %p, %p FROM %t
            ON CONFLICT ON CONSTRAINT vertex_metadata_pkey
            DO UPDATE SET value=%p
            ",
            vec![
                Box::new(name.to_string()),
                Box::new(value.clone()),
                Box::new(value.clone()),
            ],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }

    fn delete_vertex_metadata(&self, q: &VertexQuery, name: &str) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.vertex_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "DELETE FROM vertex_metadata WHERE owner_id IN (SELECT id FROM %t) AND name=%p",
            vec![Box::new(name.to_string())],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }

    fn get_edge_metadata(&self, q: &EdgeQuery, name: &str) -> Result<Vec<models::EdgeMetadata>> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.edge_query_to_sql(q, &mut sql_query_builder);

        let (query, params) = sql_query_builder.into_query_payload(
            "
            SELECT edges.outbound_id, edges.type, edges.inbound_id, edge_metadata.value
            FROM edge_metadata JOIN edges ON edge_metadata.owner_id=edges.id
            WHERE owner_id IN (SELECT id FROM %t) AND name=%p
            ",
            vec![Box::new(name.to_string())],
        );

        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        let results = self.trans.query(&query[..], &params_refs[..])?;
        let mut metadata = Vec::new();

        for row in &results {
            let outbound_id: Uuid = row.get(0);
            let t_str: String = row.get(1);
            let inbound_id: Uuid = row.get(2);
            let value: JsonValue = row.get(3);
            let t = models::Type::new(t_str).unwrap();
            let key = models::EdgeKey::new(outbound_id, t, inbound_id);
            metadata.push(models::EdgeMetadata::new(key, value));
        }

        Ok(metadata)
    }

    fn set_edge_metadata(&self, q: &EdgeQuery, name: &str, value: &JsonValue) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.edge_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "
            INSERT INTO edge_metadata (owner_id, name, value)
            SELECT id, %p, %p FROM %t
            ON CONFLICT ON CONSTRAINT edge_metadata_pkey
            DO UPDATE SET value=%p
            ",
            vec![
                Box::new(name.to_string()),
                Box::new(value.clone()),
                Box::new(value.clone()),
            ],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }

    fn delete_edge_metadata(&self, q: &EdgeQuery, name: &str) -> Result<()> {
        let mut sql_query_builder = CTEQueryBuilder::new();
        self.edge_query_to_sql(q, &mut sql_query_builder);
        let (query, params) = sql_query_builder.into_query_payload(
            "DELETE FROM edge_metadata WHERE owner_id IN (SELECT id FROM %t) AND name=%p",
            vec![Box::new(name.to_string())],
        );
        let params_refs: Vec<&ToSql> = params.iter().map(|x| &**x).collect();
        self.trans.execute(&query[..], &params_refs[..])?;
        Ok(())
    }
}
