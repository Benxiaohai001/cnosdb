use models::auth::privilege::DatabasePrivilege;
use models::auth::role::CustomTenantRole;
use models::auth::role::SystemTenantRole;
use models::auth::role::TenantRoleIdentifier;
use models::auth::user::UserDesc;
use models::auth::user::UserOptions;
use models::oid::Identifier;
use models::oid::Oid;
use models::oid::UuidGenerator;
use models::schema::DatabaseSchema;
use models::schema::TableSchema;
use models::schema::Tenant;
use models::schema::TenantOptions;

use crate::{ClusterNode, ClusterNodeId};
use openraft::EffectiveMembership;
use openraft::LogId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{from_slice, from_str};
use trace::debug;

use tokio::sync::mpsc::Sender;

use sled::Db;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use trace::info;

use crate::error::{l_r_err, s_w_err, sm_r_err, sm_w_err, StorageIOResult};
use crate::store::data;
use crate::store::key_path::KeyPath;
use models::{meta_data::*, utils};

use super::command::*;

pub type CommandResp = String;

pub fn children_fullpath(path: &str, map: Arc<sled::Db>) -> Vec<String> {
    let mut path = path.to_owned();
    if !path.ends_with('/') {
        path.push('/');
    }

    let mut list = vec![];
    for res in map.scan_prefix(path.as_bytes()) {
        match res {
            Err(_) => break,
            Ok(val) => {
                let key;
                unsafe { key = String::from_utf8_unchecked((*val.0).to_owned()) };
                match key.strip_prefix(path.as_str()) {
                    Some(val) => {
                        if val.find('/').is_some() {
                            continue;
                        }
                        if val.is_empty() {
                            continue;
                        }

                        list.push(key.clone());
                    }

                    None => break,
                }
            }
        }
    }
    list
}

pub fn get_struct<T>(key: &str, map: Arc<Db>) -> Option<T>
where
    for<'a> T: Deserialize<'a>,
{
    let val = map.get(key).ok()??;
    let info: T = from_slice(&val).ok()?;
    Some(info)
}

fn fetch_and_add_incr_id(cluster: &str, map: Arc<sled::Db>, count: u32) -> u32 {
    let id_key = KeyPath::incr_id(cluster);

    let mut id_str = "1".to_string();
    if let Some(val) = map.get(&id_key).unwrap() {
        unsafe { id_str = String::from_utf8_unchecked((*val).to_owned()) };
    }
    let id_num = from_str::<u32>(&id_str).unwrap_or(1);

    let _ = map.insert(id_key.as_bytes(), (id_num + count).to_string().as_bytes());

    id_num
}

pub fn children_data<T>(path: &str, map: Arc<Db>) -> HashMap<String, T>
where
    for<'a> T: Deserialize<'a>,
{
    let mut path = path.to_owned();
    if !path.ends_with('/') {
        path.push('/');
    }
    let mut result = HashMap::new();
    for it in children_fullpath(&path, map.clone()).iter() {
        match map.get(it) {
            Err(_) => continue,
            Ok(t) => {
                if let Some(val) = t {
                    if let Ok(info) = from_slice(&val) {
                        if let Some(key) = it.strip_prefix(path.as_str()) {
                            result.insert(key.to_string(), info);
                        }
                    }
                }
            }
        }
    }

    result
}
#[derive(Debug)]
pub struct WatchTenantMetaData {
    pub sender: Sender<bool>,

    pub cluster: String,
    pub tenant: String,

    pub delta: TenantMetaDataDelta,
}

impl WatchTenantMetaData {
    pub fn interesting(&self, cluster: &str, tenant: &str) -> bool {
        if self.cluster == cluster && self.tenant == tenant {
            return true;
        }

        false
    }
}
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct StateMachineContent {
    pub last_applied_log: Option<LogId<ClusterNodeId>>,
    pub last_membership: EffectiveMembership<ClusterNodeId, ClusterNode>,
    pub data: BTreeMap<String, String>,
}

impl From<&StateMachine> for StateMachineContent {
    fn from(state: &StateMachine) -> Self {
        let mut data_tree = BTreeMap::new();
        for entry_res in state.data().iter() {
            let entry = entry_res.expect("read db failed");

            let key: &[u8] = &entry.0;
            let value: &[u8] = &entry.1;
            data_tree.insert(
                String::from_utf8(key.to_vec()).expect("invalid key"),
                String::from_utf8(value.to_vec()).expect("invalid data"),
            );
        }
        Self {
            last_applied_log: state.get_last_applied_log().expect("last_applied_log"),
            last_membership: state.get_last_membership().expect("last_membership"),
            data: data_tree,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateMachine {
    pub db: Arc<sled::Db>,
}
impl StateMachine {
    pub(crate) fn new(db: Arc<sled::Db>) -> StateMachine {
        Self { db }
    }
    fn data(&self) -> sled::Tree {
        self.db.open_tree("data").expect("data open failed")
    }
    fn state_machine(&self) -> sled::Tree {
        self.db
            .open_tree("state_machine")
            .expect("state_machine open failed")
    }
    //todo: temp it will be removed
    pub fn version(&self) -> u64 {
        self.get_last_applied_log()
            .ok()
            .unwrap_or_default()
            .unwrap_or_default()
            .index
    }
    pub(crate) fn get_last_membership(
        &self,
    ) -> StorageIOResult<EffectiveMembership<ClusterNodeId, ClusterNode>> {
        let state_machine = self.state_machine();
        state_machine
            .get(b"last_membership")
            .map_err(sm_r_err)
            .and_then(|value| {
                value
                    .map(|v| serde_json::from_slice(&v).map_err(sm_r_err))
                    .unwrap_or_else(|| Ok(EffectiveMembership::default()))
            })
    }
    pub(crate) async fn set_last_membership(
        &self,
        membership: EffectiveMembership<ClusterNodeId, ClusterNode>,
    ) -> StorageIOResult<()> {
        let value = serde_json::to_vec(&membership).map_err(sm_w_err)?;
        let state_machine = self.state_machine();
        state_machine
            .insert(b"last_membership", value)
            .map_err(sm_w_err)?;

        state_machine
            .flush_async()
            .await
            .map_err(sm_w_err)
            .map(|_| ())
    }
    //todo:
    // fn set_last_membership_tx(
    //     &self,
    //     tx_state_machine: &sled::transaction::TransactionalTree,
    //     membership: EffectiveMembership<ClusterNodeId, ClusterNode>,
    // ) -> MetaResult<()> {
    //     let value = serde_json::to_vec(&membership).map_err(sm_r_err)?;
    //     tx_state_machine
    //         .insert(b"last_membership", value)
    //         .map_err(ct_err)?;
    //     Ok(())
    // }
    pub(crate) fn get_last_applied_log(&self) -> StorageIOResult<Option<LogId<ClusterNodeId>>> {
        let state_machine = self.state_machine();
        state_machine
            .get(b"last_applied_log")
            .map_err(l_r_err)
            .and_then(|value| {
                value
                    .map(|v| serde_json::from_slice(&v).map_err(sm_r_err))
                    .transpose()
            })
    }
    pub(crate) async fn set_last_applied_log(
        &self,
        log_id: LogId<ClusterNodeId>,
    ) -> StorageIOResult<()> {
        let value = serde_json::to_vec(&log_id).map_err(sm_w_err)?;
        let state_machine = self.state_machine();
        state_machine
            .insert(b"last_applied_log", value)
            .map_err(l_r_err)?;

        state_machine
            .flush_async()
            .await
            .map_err(l_r_err)
            .map(|_| ())
    }
    //todo:
    // fn set_last_applied_log_tx(
    //     &self,
    //     tx_state_machine: &sled::transaction::TransactionalTree,
    //     log_id: LogId<ClusterNodeId>,
    // ) -> MetaResult<()> {
    //     let value = serde_json::to_vec(&log_id).map_err(ct_err)?;
    //     tx_state_machine
    //         .insert(b"last_applied_log", value)
    //         .map_err(ct_err)?;
    //     Ok(())
    // }
    pub(crate) async fn from_serializable(
        sm: StateMachineContent,
        db: Arc<sled::Db>,
    ) -> StorageIOResult<Self> {
        let data_tree = data(&db);
        let mut batch = sled::Batch::default();
        for (key, value) in sm.data {
            batch.insert(key.as_bytes(), value.as_bytes())
        }
        data_tree.apply_batch(batch).map_err(sm_w_err)?;
        data_tree.flush_async().await.map_err(s_w_err)?;

        let r = Self { db };
        if let Some(log_id) = sm.last_applied_log {
            r.set_last_applied_log(log_id).await?;
        }
        r.set_last_membership(sm.last_membership).await?;

        Ok(r)
    }

    //todo:
    // fn insert_tx(
    //     &self,
    //     tx_data_tree: &sled::transaction::TransactionalTree,
    //     key: String,
    //     value: String,
    // ) -> MetaResult<()> {
    //     tx_data_tree
    //         .insert(key.as_bytes(), value.as_bytes())
    //         .map_err(ct_err)?;
    //     Ok(())
    // }
    pub fn get(&self, key: &str) -> StorageIOResult<Option<String>> {
        let key = key.as_bytes();
        let data_tree = self.data();
        data_tree
            .get(key)
            .map(|value| {
                value.map(|value| String::from_utf8(value.to_vec()).expect("invalid data"))
            })
            .map_err(sm_r_err)
    }
    pub fn get_all(&self) -> StorageIOResult<Vec<String>> {
        let data_tree = self.data();

        let data = data_tree
            .iter()
            .filter_map(|entry_res| {
                if let Ok(el) = entry_res {
                    Some(String::from_utf8(el.1.to_vec()).expect("invalid data"))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        Ok(data)
    }

    pub fn to_tenant_meta_data(
        &self,
        cluster: &str,
        tenant: &str,
    ) -> StorageIOResult<TenantMetaData> {
        let mut meta = TenantMetaData::new();
        meta.version = self.version();
        meta.users =
            children_data::<UserInfo>(&KeyPath::tenant_users(cluster, tenant), self.db.clone());
        // meta.data_nodes = children_data::<NodeInfo>(&KeyPath::data_nodes(cluster), self.db.clone());
        //
        let db_schemas =
            children_data::<DatabaseSchema>(&KeyPath::tenant_dbs(cluster, tenant), self.db.clone());

        for (key, schema) in db_schemas.iter() {
            let buckets = children_data::<BucketInfo>(
                &KeyPath::tenant_db_buckets(cluster, tenant, key),
                self.db.clone(),
            );

            let tables = children_data::<TableSchema>(
                &KeyPath::tenant_schemas(cluster, tenant, key),
                self.db.clone(),
            );

            let info = DatabaseInfo {
                tables,
                schema: schema.clone(),
                buckets: buckets.into_values().collect(),
            };

            meta.dbs.insert(key.clone(), info);
        }

        Ok(meta)
    }

    pub fn process_read_command(&self, req: &ReadCommand) -> CommandResp {
        info!("meta process read command {:?}", req);

        match req {
            ReadCommand::DataNodes(cluster) => {
                let response: Vec<NodeInfo> =
                    children_data::<NodeInfo>(&KeyPath::data_nodes(cluster), self.db.clone())
                        .into_values()
                        .collect();

                serde_json::to_string(&response).unwrap()
            }

            ReadCommand::TenaneMetaData(cluster, tenant) => TenaneMetaDataResp::new_from_data(
                META_REQUEST_SUCCESS,
                "".to_string(),
                self.to_tenant_meta_data(cluster, tenant).unwrap(),
            )
            .to_string(),

            ReadCommand::CustomRole(cluster, role_name, tenant_name) => {
                let path = KeyPath::role(cluster, tenant_name, role_name);

                let role = get_struct::<CustomTenantRole<Oid>>(&path, self.db.clone());

                CommonResp::Ok(role).to_string()
            }
            ReadCommand::CustomRoles(cluster, tenant_name) => {
                let path = KeyPath::roles(cluster, tenant_name);

                let roles: Vec<CustomTenantRole<Oid>> =
                    children_data::<CustomTenantRole<Oid>>(&path, self.db.clone())
                        .into_values()
                        .collect();

                CommonResp::Ok(roles).to_string()
            }

            ReadCommand::MemberRole(cluster, tenant_name, user_id) => {
                let path = KeyPath::member(cluster, tenant_name, user_id);

                let member = get_struct::<TenantRoleIdentifier>(&path, self.db.clone());

                CommonResp::Ok(member).to_string()
            }
            ReadCommand::Members(cluster, tenant_name) => {
                let path = KeyPath::members(cluster, tenant_name);

                let members = children_data::<TenantRoleIdentifier>(&path, self.db.clone());
                let users: HashMap<String, UserDesc> =
                    children_data::<UserDesc>(&KeyPath::users(cluster), self.db.clone())
                        .into_values()
                        .map(|desc| (format!("{}", desc.id()), desc))
                        .collect();

                trace::trace!("members of path {}: {:?}", path, members);
                trace::trace!("all users: {:?}", users);

                let members: HashMap<String, TenantRoleIdentifier> = members
                    .into_iter()
                    .filter_map(|(id, role)| users.get(&id).map(|e| (e.name().to_string(), role)))
                    .collect();

                debug!("returned members of path {}: {:?}", path, members);

                CommonResp::Ok(members).to_string()
            }

            ReadCommand::User(cluster, user_name) => {
                debug!("received ReadCommand::User: {}, {}", cluster, user_name);

                let path = KeyPath::user(cluster, user_name);

                let user = get_struct::<UserDesc>(&path, self.db.clone());

                CommonResp::Ok(user).to_string()
            }
            ReadCommand::Users(cluster) => {
                let path = KeyPath::users(cluster);

                let users: Vec<UserDesc> = children_data::<UserDesc>(&path, self.db.clone())
                    .into_values()
                    .collect();

                CommonResp::Ok(users).to_string()
            }
            ReadCommand::Tenant(cluster, tenant_name) => {
                debug!("received ReadCommand::Tenant: {}, {}", cluster, tenant_name);

                let path = KeyPath::tenant(cluster, tenant_name);

                let data = get_struct::<Tenant>(&path, self.db.clone());

                CommonResp::Ok(data).to_string()
            }
            ReadCommand::Tenants(cluster) => {
                let path = KeyPath::tenants(cluster);

                let data: Vec<Tenant> = children_data::<Tenant>(&path, self.db.clone())
                    .into_values()
                    .collect();

                CommonResp::Ok(data).to_string()
            }
        }
    }

    pub fn process_write_command(
        &self,
        req: &WriteCommand,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        info!("meta process write command {:?}", req);

        match req {
            WriteCommand::Set { key, value } => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                info!("WRITE: {} :{}", key, value);

                CommandResp::default()
            }

            WriteCommand::AddDataNode(cluster, node) => self.process_add_date_node(cluster, node),

            WriteCommand::CreateDB(cluster, tenant, schema) => {
                self.process_create_db(cluster, tenant, schema, watch)
            }

            WriteCommand::AlterDB(cluster, tenant, schema) => {
                self.process_alter_db(cluster, tenant, schema, watch)
            }

            WriteCommand::DropDB(cluster, tenant, db_name) => {
                self.process_drop_db(cluster, tenant, db_name, watch)
            }

            WriteCommand::DropTable(cluster, tenant, db_name, table_name) => {
                self.process_drop_table(cluster, tenant, db_name, table_name, watch)
            }

            WriteCommand::CreateTable(cluster, tenant, schema) => {
                self.process_create_table(cluster, tenant, schema, watch)
            }

            WriteCommand::UpdateTable(cluster, tenant, schema) => {
                self.process_update_table(cluster, tenant, schema, watch)
            }

            WriteCommand::CreateBucket(cluster, tenant, db, ts) => {
                self.process_create_bucket(cluster, tenant, db, ts, watch)
            }

            WriteCommand::DeleteBucket(cluster, tenant, db, id) => {
                self.process_delete_bucket(cluster, tenant, db, *id, watch)
            }

            WriteCommand::CreateUser(cluster, name, options, is_admin) => {
                self.process_create_user(cluster, name, options, *is_admin)
            }
            WriteCommand::AlterUser(cluster, name, options) => {
                self.process_alter_user(cluster, name, options)
            }
            WriteCommand::RenameUser(cluster, old_name, new_name) => {
                self.process_rename_user(cluster, old_name, new_name)
            }
            WriteCommand::DropUser(cluster, name) => self.process_drop_user(cluster, name),

            WriteCommand::CreateTenant(cluster, name, options) => {
                self.process_create_tenant(cluster, name, options)
            }
            WriteCommand::AlterTenant(cluster, name, options) => {
                self.process_alter_tenant(cluster, name, options)
            }
            WriteCommand::RenameTenant(cluster, old_name, new_name) => {
                self.process_rename_tenant(cluster, old_name, new_name)
            }
            WriteCommand::DropTenant(cluster, name) => self.process_drop_tenant(cluster, name),

            WriteCommand::AddMemberToTenant(cluster, user_id, role, tenant_name) => {
                self.process_add_member_to_tenant(cluster, user_id, role, tenant_name)
            }
            WriteCommand::RemoveMemberFromTenant(cluster, user_id, tenant_name) => {
                self.process_remove_member_to_tenant(cluster, user_id, tenant_name)
            }
            WriteCommand::ReasignMemberRole(cluster, user_id, role, tenant_name) => {
                self.process_reasign_member_role(cluster, user_id, role, tenant_name)
            }

            WriteCommand::CreateRole(cluster, role_name, sys_role, privileges, tenant_name) => {
                self.process_create_role(cluster, role_name, sys_role, privileges, tenant_name)
            }
            WriteCommand::DropRole(cluster, role_name, tenant_name) => {
                self.process_drop_role(cluster, role_name, tenant_name)
            }
            WriteCommand::GrantPrivileges(cluster, privileges, role_name, tenant_name) => {
                self.process_grant_privileges(cluster, privileges, role_name, tenant_name)
            }
            WriteCommand::RevokePrivileges(cluster, privileges, role_name, tenant_name) => {
                self.process_revoke_privileges(cluster, privileges, role_name, tenant_name)
            }
        }
    }

    fn process_add_date_node(&self, cluster: &str, node: &NodeInfo) -> CommandResp {
        let key = KeyPath::data_node_id(cluster, node.id);
        let value = serde_json::to_string(node).unwrap();
        let _ = self.db.insert(key.as_bytes(), value.as_bytes());
        info!("WRITE: {} :{}", key, value);

        serde_json::to_string(&StatusResponse::default()).unwrap()
    }

    fn process_drop_db(
        &self,
        cluster: &str,
        tenant: &str,
        db_name: &str,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_db_name(cluster, tenant, db_name);
        let _ = self.db.remove(key);

        let buckets_path = KeyPath::tenant_db_buckets(cluster, tenant, db_name);
        for it in children_fullpath(&buckets_path, self.db.clone()).iter() {
            let _ = self.db.remove(it);
        }

        let schemas_path = KeyPath::tenant_schemas(cluster, tenant, db_name);
        for it in children_fullpath(&schemas_path, self.db.clone()).iter() {
            let _ = self.db.remove(it);
        }

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.delete_db(self.version(), db_name);
                let _ = item.sender.try_send(true);
            }
        }

        StatusResponse::new(META_REQUEST_SUCCESS, "".to_string()).to_string()
    }

    fn process_drop_table(
        &self,
        cluster: &str,
        tenant: &str,
        db_name: &str,
        table_name: &str,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_schema_name(cluster, tenant, db_name, table_name);
        let _ = self.db.remove(key);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.delete_table(self.version(), db_name, table_name);
                let _ = item.sender.try_send(true);
            }
        }

        StatusResponse::new(META_REQUEST_SUCCESS, "".to_string()).to_string()
    }

    fn process_create_db(
        &self,
        cluster: &str,
        tenant: &str,
        schema: &DatabaseSchema,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_db_name(cluster, tenant, schema.database_name());
        if self.db.contains_key(&key).unwrap() {
            return TenaneMetaDataResp::new_from_data(
                META_REQUEST_DB_EXIST,
                "database already exist".to_string(),
                self.to_tenant_meta_data(cluster, tenant).unwrap(),
            )
            .to_string();
        }

        let value = serde_json::to_string(schema).unwrap();
        let _ = self.db.insert(key.as_bytes(), value.as_bytes());
        info!("WRITE: {} :{}", key, value);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.create_db(self.version(), schema);
                let _ = item.sender.try_send(true);
            }
        }

        TenaneMetaDataResp::new_from_data(
            META_REQUEST_SUCCESS,
            "".to_string(),
            self.to_tenant_meta_data(cluster, tenant).unwrap(),
        )
        .to_string()
    }

    fn process_alter_db(
        &self,
        cluster: &str,
        tenant: &str,
        schema: &DatabaseSchema,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_db_name(cluster, tenant, schema.database_name());
        if !self.db.contains_key(&key).unwrap() {
            return StatusResponse::new(META_REQUEST_SUCCESS, "db not found in meta".to_string())
                .to_string();
        }

        let value = serde_json::to_string(schema).unwrap();
        let _ = self.db.insert(key.as_bytes(), value.as_bytes());
        info!("WRITE: {} :{}", key, value);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.update_db_schema(self.version(), schema);
                let _ = item.sender.try_send(true);
            }
        }

        StatusResponse::new(META_REQUEST_SUCCESS, "".to_string()).to_string()
    }

    fn process_create_table(
        &self,
        cluster: &str,
        tenant: &str,
        schema: &TableSchema,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_db_name(cluster, tenant, &schema.db());
        if !self.db.contains_key(key).unwrap() {
            return TenaneMetaDataResp::new_from_data(
                META_REQUEST_DB_EXIST,
                "database not found".to_string(),
                self.to_tenant_meta_data(cluster, tenant).unwrap(),
            )
            .to_string();
        }
        let key = KeyPath::tenant_schema_name(cluster, tenant, &schema.db(), &schema.name());
        if self.db.contains_key(&key).unwrap() {
            return TenaneMetaDataResp::new_from_data(
                META_REQUEST_TABLE_EXIST,
                "table already exist".to_string(),
                self.to_tenant_meta_data(cluster, tenant).unwrap(),
            )
            .to_string();
        }

        let value = serde_json::to_string(schema).unwrap();
        let _ = self.db.insert(key.as_bytes(), value.as_bytes()).unwrap();
        info!("WRITE: {} :{}", key, value);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.create_or_update_table(self.version(), schema);
                let _ = item.sender.try_send(true);
            }
        }

        TenaneMetaDataResp::new_from_data(
            META_REQUEST_SUCCESS,
            "".to_string(),
            self.to_tenant_meta_data(cluster, tenant).unwrap(),
        )
        .to_string()
    }

    fn process_update_table(
        &self,
        cluster: &str,
        tenant: &str,
        schema: &TableSchema,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_schema_name(cluster, tenant, &schema.db(), &schema.name());
        if let Some(val) = get_struct::<TableSchema>(&key, self.db.clone()) {
            match (val, schema) {
                (TableSchema::TsKvTableSchema(val), TableSchema::TsKvTableSchema(schema)) => {
                    if val.schema_id + 1 != schema.schema_id {
                        return TenaneMetaDataResp::new_from_data(
                            META_REQUEST_FAILED,
                            format!(
                                "update table schema conflict {}->{}",
                                val.schema_id, schema.schema_id
                            ),
                            self.to_tenant_meta_data(cluster, tenant).unwrap(),
                        )
                        .to_string();
                    }
                }
                _ => {
                    return TenaneMetaDataResp::new_from_data(
                        META_REQUEST_FAILED,
                        "not support update external table".to_string(),
                        self.to_tenant_meta_data(cluster, tenant).unwrap(),
                    )
                    .to_string()
                }
            }
        }

        let value = serde_json::to_string(schema).unwrap();
        let _ = self.db.insert(key.as_bytes(), value.as_bytes());
        info!("WRITE: {} :{}", key, value);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.create_or_update_table(self.version(), schema);
                let _ = item.sender.try_send(true);
            }
        }

        TenaneMetaDataResp::new_from_data(
            META_REQUEST_SUCCESS,
            "".to_string(),
            self.to_tenant_meta_data(cluster, tenant).unwrap(),
        )
        .to_string()
    }

    fn process_create_bucket(
        &self,
        cluster: &str,
        tenant: &str,
        db: &str,
        ts: &i64,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let db_path = KeyPath::tenant_db_name(cluster, tenant, db);
        let buckets = children_data::<BucketInfo>(&(db_path.clone() + "/buckets"), self.db.clone());
        for (_, val) in buckets.iter() {
            if *ts >= val.start_time && *ts < val.end_time {
                return TenaneMetaDataResp::new_from_data(
                    META_REQUEST_SUCCESS,
                    "".to_string(),
                    self.to_tenant_meta_data(cluster, tenant).unwrap(),
                )
                .to_string();
            }
        }
        let res = self
            .db
            .get(&db_path)
            .unwrap()
            .and_then(|v| from_slice::<DatabaseSchema>(&v).ok());
        let db_schema = match res {
            Some(info) => info,
            None => {
                return TenaneMetaDataResp::new(
                    META_REQUEST_FAILED,
                    format!("database {} is not exist", db),
                )
                .to_string();
            }
        };

        let node_list: Vec<NodeInfo> =
            children_data::<NodeInfo>(&KeyPath::data_nodes(cluster), self.db.clone())
                .into_values()
                .collect();

        let now = utils::now_timestamp();
        if node_list.is_empty()
            || db_schema.config.shard_num_or_default() == 0
            || db_schema.config.replica_or_default() > node_list.len() as u64
        {
            return TenaneMetaDataResp::new(
                META_REQUEST_FAILED,
                format!("database {} attribute invalid!", db),
            )
            .to_string();
        }

        if *ts < now - db_schema.config.ttl_or_default().time_stamp() {
            return TenaneMetaDataResp::new(
                META_REQUEST_FAILED,
                format!("database {} create expired bucket not permit!", db),
            )
            .to_string();
        }

        let mut bucket = BucketInfo {
            id: fetch_and_add_incr_id(cluster, self.db.clone(), 1),
            start_time: 0,
            end_time: 0,
            shard_group: vec![],
        };
        (bucket.start_time, bucket.end_time) = get_time_range(
            *ts,
            db_schema.config.vnode_duration_or_default().time_stamp(),
        );
        let (group, used) = allocation_replication_set(
            node_list,
            db_schema.config.shard_num_or_default() as u32,
            db_schema.config.replica_or_default() as u32,
            bucket.id + 1,
        );
        bucket.shard_group = group;
        fetch_and_add_incr_id(cluster, self.db.clone(), used);

        let key = KeyPath::tenant_bucket_id(cluster, tenant, db, bucket.id);
        let val = serde_json::to_string(&bucket).unwrap();

        let _ = self.db.insert(key.as_bytes(), val.as_bytes()).unwrap();
        info!("WRITE: {} :{}", key, val);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta
                    .create_or_update_bucket(self.version(), db, &bucket);
                let _ = item.sender.try_send(true);
            }
        }

        TenaneMetaDataResp::new_from_data(
            META_REQUEST_SUCCESS,
            "".to_string(),
            self.to_tenant_meta_data(cluster, tenant).unwrap(),
        )
        .to_string()
    }

    fn process_delete_bucket(
        &self,
        cluster: &str,
        tenant: &str,
        db: &str,
        id: u32,
        watch: &mut HashMap<String, WatchTenantMetaData>,
    ) -> CommandResp {
        let key = KeyPath::tenant_bucket_id(cluster, tenant, db, id);
        let _ = self.db.remove(key);

        for (_, item) in watch.iter_mut() {
            if item.interesting(cluster, tenant) {
                item.delta.delete_bucket(self.version(), db, id);
                let _ = item.sender.try_send(true);
            }
        }

        StatusResponse::new(META_REQUEST_SUCCESS, "".to_string()).to_string()
    }

    fn process_create_user(
        &self,
        cluster: &str,
        user_name: &str,
        user_options: &UserOptions,
        is_admin: bool,
    ) -> CommandResp {
        let key = KeyPath::user(cluster, user_name);

        if self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(META_REQUEST_USER_EXIST, user_name.to_string());
            return CommonResp::<Oid>::Err(status).to_string();
        }

        let oid = UuidGenerator::default().next_id();
        let user_desc = UserDesc::new(oid, user_name.to_string(), user_options.clone(), is_admin);

        match serde_json::to_string(&user_desc) {
            Ok(value) => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                CommonResp::Ok(oid).to_string()
            }
            Err(err) => {
                let status = StatusResponse::new(META_REQUEST_FAILED, err.to_string());
                CommonResp::<Oid>::Err(status).to_string()
            }
        }
    }

    fn process_alter_user(
        &self,
        cluster: &str,
        user_name: &str,
        user_options: &UserOptions,
    ) -> CommandResp {
        let key = KeyPath::user(cluster, user_name);

        let resp = if let Some(e) = self.db.remove(&key).unwrap() {
            match serde_json::from_slice::<UserDesc>(&e) {
                Ok(old_user_desc) => {
                    let old_options = old_user_desc.options().to_owned();
                    let new_options = old_options.merge(user_options.clone());

                    let new_user_desc = UserDesc::new(
                        *old_user_desc.id(),
                        user_name.to_string(),
                        new_options,
                        old_user_desc.is_admin(),
                    );
                    let value = serde_json::to_string(&new_user_desc).unwrap();
                    let _ = self.db.insert(key.as_bytes(), value.as_bytes());

                    CommonResp::Ok(())
                }
                Err(err) => {
                    CommonResp::Err(StatusResponse::new(META_REQUEST_FAILED, err.to_string()))
                }
            }
        } else {
            CommonResp::Err(StatusResponse::new(
                META_REQUEST_USER_NOT_FOUND,
                user_name.to_string(),
            ))
        };

        resp.to_string()
    }

    fn process_rename_user(&self, _cluster: &str, _old_name: &str, _new_name: &str) -> CommandResp {
        let status = StatusResponse::new(META_REQUEST_FAILED, "Not implement".to_string());
        CommonResp::<()>::Err(status).to_string()
    }

    fn process_drop_user(&self, cluster: &str, user_name: &str) -> CommandResp {
        let key = KeyPath::user(cluster, user_name);

        let success = self.db.remove(key).is_ok();

        CommonResp::Ok(success).to_string()
    }

    fn process_create_tenant(
        &self,
        cluster: &str,
        name: &str,
        options: &TenantOptions,
    ) -> CommandResp {
        let key = KeyPath::tenant(cluster, name);

        if self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(META_REQUEST_TENANT_EXIST, name.to_string());
            return CommonResp::<Tenant>::Err(status).to_string();
        }

        let oid = UuidGenerator::default().next_id();
        let tenant = Tenant::new(oid, name.to_string(), options.clone());

        match serde_json::to_string(&tenant) {
            Ok(value) => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                CommonResp::Ok(tenant).to_string()
            }
            Err(err) => {
                let status = StatusResponse::new(META_REQUEST_FAILED, err.to_string());
                CommonResp::<Tenant>::Err(status).to_string()
            }
        }
    }

    fn process_alter_tenant(
        &self,
        cluster: &str,
        name: &str,
        options: &TenantOptions,
    ) -> CommandResp {
        let key = KeyPath::tenant(cluster, name);

        let resp = if let Some(e) = self.db.remove(&key).unwrap() {
            match serde_json::from_slice::<Tenant>(&e) {
                Ok(tenant) => {
                    let new_tenant =
                        Tenant::new(*tenant.id(), name.to_string(), options.to_owned());
                    let value = serde_json::to_string(&new_tenant).unwrap();
                    let _ = self.db.insert(key.as_bytes(), value.as_bytes());

                    CommonResp::Ok(())
                }
                Err(err) => {
                    CommonResp::Err(StatusResponse::new(META_REQUEST_FAILED, err.to_string()))
                }
            }
        } else {
            CommonResp::Err(StatusResponse::new(
                META_REQUEST_TENANT_NOT_FOUND,
                name.to_string(),
            ))
        };

        resp.to_string()
    }

    fn process_rename_tenant(
        &self,
        _cluster: &str,
        _old_name: &str,
        _new_name: &str,
    ) -> CommandResp {
        let status = StatusResponse::new(META_REQUEST_FAILED, "Not implement".to_string());
        CommonResp::<()>::Err(status).to_string()
    }

    fn process_drop_tenant(&self, cluster: &str, name: &str) -> CommandResp {
        let key = KeyPath::tenant(cluster, name);

        let success = self.db.remove(key).is_ok();

        CommonResp::Ok(success).to_string()
    }

    fn process_add_member_to_tenant(
        &self,
        cluster: &str,
        user_id: &Oid,
        role: &TenantRoleIdentifier,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::member(cluster, tenant_name, user_id);

        if self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(META_REQUEST_USER_EXIST, user_id.to_string());
            return CommonResp::<()>::Err(status).to_string();
        }

        match serde_json::to_string(role) {
            Ok(value) => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                CommonResp::Ok(()).to_string()
            }
            Err(err) => {
                let status = StatusResponse::new(META_REQUEST_FAILED, err.to_string());
                CommonResp::<()>::Err(status).to_string()
            }
        }
    }

    fn process_remove_member_to_tenant(
        &self,
        cluster: &str,
        user_id: &Oid,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::member(cluster, tenant_name, user_id);

        if self.db.remove(key).unwrap().is_none() {
            let status = StatusResponse::new(META_REQUEST_USER_NOT_FOUND, user_id.to_string());
            return CommonResp::<()>::Err(status).to_string();
        }

        CommonResp::Ok(()).to_string()
    }

    fn process_reasign_member_role(
        &self,
        cluster: &str,
        user_id: &Oid,
        role: &TenantRoleIdentifier,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::member(cluster, tenant_name, user_id);

        if self.db.remove(&key).unwrap().is_none() {
            let status = StatusResponse::new(META_REQUEST_USER_NOT_FOUND, user_id.to_string());
            return CommonResp::<()>::Err(status).to_string();
        }

        match serde_json::to_string(role) {
            Ok(value) => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                CommonResp::Ok(()).to_string()
            }
            Err(err) => {
                let status = StatusResponse::new(META_REQUEST_FAILED, err.to_string());
                CommonResp::<()>::Err(status).to_string()
            }
        }
    }

    fn process_create_role(
        &self,
        cluster: &str,
        role_name: &str,
        sys_role: &SystemTenantRole,
        privileges: &HashMap<String, DatabasePrivilege>,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::role(cluster, tenant_name, role_name);

        if self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(
                META_REQUEST_ROLE_EXIST,
                format!("{} of tenant {}", role_name, tenant_name),
            );
            return CommonResp::<()>::Err(status).to_string();
        }

        let oid = UuidGenerator::default().next_id();
        let role = CustomTenantRole::new(
            oid,
            role_name.to_string(),
            sys_role.clone(),
            privileges.clone(),
        );

        match serde_json::to_string(&role) {
            Ok(value) => {
                let _ = self.db.insert(key.as_bytes(), value.as_bytes());
                CommonResp::Ok(()).to_string()
            }
            Err(err) => {
                let status = StatusResponse::new(META_REQUEST_FAILED, err.to_string());
                CommonResp::<()>::Err(status).to_string()
            }
        }
    }

    fn process_drop_role(&self, cluster: &str, role_name: &str, tenant_name: &str) -> CommandResp {
        let key = KeyPath::role(cluster, tenant_name, role_name);

        let success = self.db.remove(key).unwrap().is_some();

        CommonResp::Ok(success).to_string()
    }

    fn process_grant_privileges(
        &self,
        cluster: &str,
        privileges: &[(DatabasePrivilege, String)],
        role_name: &str,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::role(cluster, tenant_name, role_name);

        if !self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(
                META_REQUEST_ROLE_NOT_FOUND,
                format!("{} of tenant {}", role_name, tenant_name),
            );
            return CommonResp::<()>::Err(status).to_string();
        }

        let val = self.db.get(&key).unwrap().map(|e| {
            let mut old_role =
                unsafe { serde_json::from_slice::<CustomTenantRole<Oid>>(&e).unwrap_unchecked() };
            for (privilege, database_name) in privileges {
                let _ = old_role.grant_privilege(database_name.clone(), privilege.clone());
            }

            unsafe { serde_json::to_string(&old_role).unwrap_unchecked() }
        });
        let _ = self.db.insert(key.as_bytes(), val.unwrap().as_bytes());

        CommonResp::Ok(()).to_string()
    }

    fn process_revoke_privileges(
        &self,
        cluster: &str,
        privileges: &[(DatabasePrivilege, String)],
        role_name: &str,
        tenant_name: &str,
    ) -> CommandResp {
        let key = KeyPath::role(cluster, tenant_name, role_name);

        if !self.db.contains_key(&key).unwrap() {
            let status = StatusResponse::new(
                META_REQUEST_ROLE_NOT_FOUND,
                format!("{} of tenant {}", role_name, tenant_name),
            );
            return CommonResp::<()>::Err(status).to_string();
        }

        let val = self.db.get(&key).unwrap().map(|e| {
            let mut old_role =
                unsafe { serde_json::from_slice::<CustomTenantRole<Oid>>(&e).unwrap_unchecked() };
            for (privilege, database_name) in privileges {
                let _ = old_role.revoke_privilege(database_name, privilege);
            }

            unsafe { serde_json::to_string(&old_role).unwrap_unchecked() }
        });

        let _ = self.db.insert(key.as_bytes(), val.unwrap().as_bytes());

        CommonResp::Ok(()).to_string()
    }
}

#[cfg(test)]
mod test {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::println;

    #[tokio::test]
    async fn test_btree_map() {
        let mut map = BTreeMap::new();
        map.insert("/root/tenant".to_string(), "tenant_v".to_string());
        map.insert("/root/tenant/db1".to_string(), "123_v".to_string());
        map.insert("/root/tenant/db2".to_string(), "456_v".to_string());
        map.insert("/root/tenant/db1/".to_string(), "123/_v".to_string());
        map.insert("/root/tenant/db1/table1".to_string(), "123_v".to_string());
        map.insert("/root/tenant/123".to_string(), "123_v".to_string());
        map.insert("/root/tenant/456".to_string(), "456_v".to_string());

        let begin = "/root/tenant/".to_string();
        let end = "/root/tenant/|".to_string();
        for (key, value) in map.range(begin..end) {
            println!("{key}  : {value}");
        }
    }

    //{"Set":{"key":"foo","value":"bar111"}}
    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Command1 {
        id: u32,
        name: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct Command2 {
        id: u32,
        name: String,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub enum Command {
        // Test1 { id: u32, name: String },
        // Test2 { id: u32, name: String },
        Test1(Command1),
    }

    #[tokio::test]
    async fn test_json() {
        let cmd = Command::Test1(Command1 {
            id: 100,
            name: "test".to_string(),
        });

        let str = serde_json::to_vec(&cmd).unwrap();
        print!("\n1 === {}=== \n", String::from_utf8(str).unwrap());

        let str = serde_json::to_string(&cmd).unwrap();
        print!("\n2 === {}=== \n", str);

        let tup = ("test1".to_string(), "test2".to_string());
        let str = serde_json::to_string(&tup).unwrap();
        print!("\n3 === {}=== \n", str);

        let str = serde_json::to_string(&"xxx".to_string()).unwrap();
        print!("\n4 === {}=== \n", str);
    }
}