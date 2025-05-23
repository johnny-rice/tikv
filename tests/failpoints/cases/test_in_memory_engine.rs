// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    fs::File,
    io::Read,
    sync::{Arc, Mutex, mpsc::sync_channel},
    thread::sleep,
    time::Duration,
};

use engine_rocks::RocksSstWriterBuilder;
use engine_traits::{
    CF_DEFAULT, CacheRegion, DATA_CFS, EvictReason, RegionCacheEngine, RegionCacheEngineExt,
    SstWriter, SstWriterBuilder,
};
use file_system::calc_crc32_bytes;
use in_memory_engine::test_util::new_region;
use keys::{DATA_MAX_KEY, DATA_MIN_KEY, data_key};
use kvproto::{
    import_sstpb::SstMeta,
    kvrpcpb::Context,
    raft_cmdpb::{AdminCmdType, CmdType, RaftCmdRequest, RaftRequestHeader, Request},
    raft_serverpb::{PeerState, RaftMessage},
};
use pd_client::PdClient;
use protobuf::Message;
use raft::eraftpb::MessageType;
use raftstore::{
    coprocessor::ObserveHandle,
    store::{
        fsm::{ApplyTask, apply::ChangeObserver},
        msg::Callback,
    },
};
use tempfile::tempdir;
use test_coprocessor::{
    DagChunkSpliter, DagSelect, ProductTable, handle_request, init_data_with_details_pd_client,
};
use test_raftstore::{
    CloneFilterFactory, Cluster, Direction, RegionPacketFilter, ServerCluster, configure_for_merge,
    get_tso, must_get_equal, new_learner_peer, new_peer, new_put_cf_cmd,
    new_server_cluster_with_hybrid_engine, sleep_ms,
};
use test_util::eventually;
use tidb_query_datatype::{
    codec::{Datum, datum},
    expr::EvalContext,
};
use tikv_util::{HandyRwLock, mpsc::unbounded};
use tipb::SelectResponse;
use txn_types::Key;

fn copr_point_get(
    cluster: &mut Cluster<ServerCluster>,
    table: &ProductTable,
    row_id: i64,
) -> SelectResponse {
    let key = table.get_table_prefix();
    let table_key = Key::from_raw(&key).into_encoded();
    let ctx = cluster.get_ctx(&table_key);
    let endpoint = cluster.sim.rl().copr_endpoints[&1].clone();
    let key_range = table.get_record_range_one(row_id);
    let req = DagSelect::from(table)
        .key_ranges(vec![key_range])
        .start_ts(get_tso(&cluster.pd_client).into())
        .build_with(ctx, &[0]);
    let cop_resp = handle_request(&endpoint, req);
    assert!(!cop_resp.has_region_error(), "{:?}", cop_resp);
    assert!(cop_resp.get_other_error().is_empty(), "{:?}", cop_resp);
    let mut resp = SelectResponse::default();
    resp.merge_from_bytes(cop_resp.get_data()).unwrap();
    resp
}

fn must_copr_point_get(cluster: &mut Cluster<ServerCluster>, table: &ProductTable, row_id: i64) {
    let mut resp = copr_point_get(cluster, table, row_id);
    let mut spliter = DagChunkSpliter::new(resp.take_chunks().into(), 3);
    let row = spliter.next().unwrap();
    let expected_encoded = datum::encode_value(
        &mut EvalContext::default(),
        &[
            Datum::I64(row_id),
            Some(format!("name:{}", row_id).into_bytes()).into(),
            row_id.into(),
        ],
    )
    .unwrap();
    let result_encoded = datum::encode_value(&mut EvalContext::default(), &row).unwrap();
    assert_eq!(result_encoded, &*expected_encoded);

    assert!(spliter.next().is_none());
}

fn must_copr_point_get_empty(
    cluster: &mut Cluster<ServerCluster>,
    table: &ProductTable,
    row_id: i64,
) {
    let mut resp = copr_point_get(cluster, table, row_id);
    let mut spliter = DagChunkSpliter::new(resp.take_chunks().into(), 3);
    assert!(spliter.next().is_none());
}

fn must_copr_load_data(cluster: &mut Cluster<ServerCluster>, table: &ProductTable, row_id: i64) {
    let key = table.get_table_prefix();
    let table_key = Key::from_raw(&key).into_encoded();
    let ctx = cluster.get_ctx(&table_key);
    let engine = cluster.sim.rl().storages[&1].clone();
    let _ = init_data_with_details_pd_client(
        ctx.clone(),
        engine,
        table,
        &[(row_id, Some(&format!("name:{}", row_id)), row_id)],
        true,
        &cluster.cfg.tikv.server,
        Some(cluster.pd_client.clone()),
    );
}

fn async_put(
    cluster: &mut Cluster<ServerCluster>,
    table: &ProductTable,
    row_id: i64,
) -> std::thread::JoinHandle<()> {
    let cfg = cluster.cfg.tikv.server.clone();
    let pd_client = cluster.pd_client.clone();
    let key = table.get_table_prefix();
    let split_key = Key::from_raw(&key).into_encoded();
    let ctx = cluster.get_ctx(&split_key);
    let engine = cluster.sim.rl().storages[&ctx.get_peer().get_store_id()].clone();
    let table_ = table.clone();
    let (tx, rx) = unbounded();
    let handle = std::thread::spawn(move || {
        tx.send(()).unwrap();
        let _ = init_data_with_details_pd_client(
            ctx,
            engine,
            &table_,
            &[(row_id, Some(&format!("name:{}", row_id)), row_id)],
            true,
            &cfg,
            Some(pd_client),
        );
    });
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle
}

#[test]
fn test_put_copr_get() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
    cluster.run();

    let current_ts = get_tso(&cluster.pd_client);
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    // FIXME: load is not implemented, so we have to insert range manually
    {
        let region = cluster.get_region(b"k");
        let rid = region.id;
        region_cache_engine
            .core()
            .region_manager()
            .new_region(CacheRegion::from_region(&region));
        region_cache_engine
            .core()
            .region_manager()
            .set_safe_point(rid, current_ts);
    }

    let product = ProductTable::new();
    must_copr_load_data(&mut cluster, &product, 1);
    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    must_copr_point_get(&mut cluster, &product, 1);

    // verify it's read from in memory engine
    rx.try_recv().unwrap();
}

#[test]
fn test_load() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 2;
    cluster.run();

    let mut tables = vec![];
    for _ in 0..3 {
        let product = ProductTable::new();
        tables.push(product.clone());
        must_copr_load_data(&mut cluster, &product, 1);
    }

    let mut split_keys = vec![];
    for table in &tables[1..] {
        let key = table.get_table_prefix();
        let split_key = Key::from_raw(&key).into_encoded();
        let r = cluster.get_region(&split_key);
        cluster.must_split(&r, &split_key);
        split_keys.push(split_key);
    }

    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    // load range
    {
        let r = cluster.get_region(b"");
        let r1 = cluster.get_region(&split_keys[0]);
        let r2 = cluster.get_region(&split_keys[1]);
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        region_cache_engine
            .load_region(CacheRegion::from_region(&r))
            .unwrap();
        region_cache_engine
            .load_region(CacheRegion::from_region(&r1))
            .unwrap();
        region_cache_engine
            .load_region(CacheRegion::from_region(&r2))
            .unwrap();
    }

    // put key to trigger load task
    for table in &tables {
        must_copr_load_data(&mut cluster, table, 1);
    }

    // ensure the snapshot is loaded
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();

    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    for table in &tables {
        must_copr_point_get(&mut cluster, table, 1);

        // verify it's read from in memory engine
        assert!(rx.try_recv().unwrap());
    }
}

// It tests that after we schedule the pending range to load snapshot, the range
// splits.
#[test]
fn test_load_with_split() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 2;
    cluster.run();

    let (tx, rx) = sync_channel(0);
    // let channel to make load process block at finishing loading snapshot
    let (tx2, rx2) = sync_channel(0);
    let rx2 = Arc::new(Mutex::new(rx2));
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        tx.send(true).unwrap();
        let _ = rx2.lock().unwrap().recv().unwrap();
    })
    .unwrap();

    // load range
    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        // Load the whole range as if it is not splitted. Loading process should handle
        // it correctly.
        let cache_range = new_region(1, "", "");
        region_cache_engine
            .load_region(CacheRegion::from_region(&cache_range))
            .unwrap();
    }

    let mut tables = vec![];
    for _ in 0..3 {
        let product = ProductTable::new();
        tables.push(product.clone());
        must_copr_load_data(&mut cluster, &product, 1);
    }

    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Now, the snapshot load is finished, and blocked before consuming cached
    // write batches. Let split the range.

    for table in &tables[1..] {
        let key = table.get_table_prefix();
        let split_key = Key::from_raw(&key).into_encoded();
        let r = cluster.get_region(&split_key);
        cluster.must_split(&r, &split_key);
    }
    // Now, we have 3 regions: [min, table1), [table1, table2), [table2, max)

    for table in &tables {
        must_copr_load_data(&mut cluster, table, 1);
    }

    // unblock loading task
    tx2.send(true).unwrap();

    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    for table in &tables {
        must_copr_point_get(&mut cluster, table, 1);

        // verify it's read from in memory engine
        assert!(rx.try_recv().unwrap());
    }
}

// It tests race between split and load.
// Takes table1-table2 as an example:
// We want to load table1-table2 where table1-table2 is already split into
// ["", table2), and [table2, "").
// And before we `load_region` table1-table2, ["", table2) has cached some
// writes, say table1_1, in write_batch which means table1_1 cannot be loaded
// from snapshot. Now, `load_region` table1-table2 is called, and [table2, "")
// calls prepare_for_apply and the snapshot is acquired and load task of
// table1-table2 is scheduled.
#[test]
fn test_load_with_split2() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 4;
    cluster.run();
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);

    let product1 = ProductTable::new();
    let product2 = ProductTable::new();

    for table in [&product1, &product2] {
        must_copr_load_data(&mut cluster, table, 1);
    }

    let key = product2.get_table_prefix();
    let split_key = Key::from_raw(&key).into_encoded();
    let r = cluster.get_region(&split_key);
    cluster.must_split(&r, &split_key);
    let r_split = cluster.get_region(&split_key);

    let (handle_put_tx, handle_put_rx) = unbounded();
    let (handle_put_pause_tx, handle_put_pause_rx) = unbounded::<()>();
    fail::cfg_callback("on_handle_put", move || {
        handle_put_tx.send(()).unwrap();
        let _ = handle_put_pause_rx.recv();
    })
    .unwrap();
    let handle1 = async_put(&mut cluster, &product1, 2);
    handle_put_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    std::thread::sleep(Duration::from_secs(1));
    // try to load a region with old epoch and bigger range,
    // it should be updated to the real region range.
    region_cache_engine
        .load_region(CacheRegion::new(r_split.id, 0, DATA_MIN_KEY, DATA_MAX_KEY))
        .unwrap();

    let (tx, rx) = sync_channel(1);
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    let handle2 = async_put(&mut cluster, &product2, 9);
    let _ = rx.recv_timeout(Duration::from_secs(5)).unwrap();

    drop(handle_put_pause_tx);

    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        let regions_map = region_cache_engine
            .core()
            .region_manager()
            .regions_map()
            .read();
        let meta = regions_map.region_meta(r_split.id).unwrap();
        let split_range = CacheRegion::from_region(&r_split);
        assert_eq!(&split_range, meta.get_region());
    }

    fail::remove("on_handle_put");
    std::thread::sleep(Duration::from_secs(1));
    handle1.join().unwrap();
    handle2.join().unwrap();

    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();
    must_copr_point_get(&mut cluster, &product2, 9);
    assert!(rx.try_recv().unwrap());

    // ["", table2) should not cached.
    must_copr_load_data(&mut cluster, &product1, 3);
    let (tx, rx) = unbounded();
    fail::remove("ime_on_iterator_seek");
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();
    must_copr_point_get(&mut cluster, &product1, 2);
    rx.try_recv().unwrap_err();
}

// It tests that for a apply delegate, at the time it prepares to apply
// something, the range of it is in pending range. When it begins to write the
// write batch to engine, the range has finished the loading, became a normal
// range, and even been evicted.
#[test]
fn test_load_with_eviction() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.run();
    // load range
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    // Load the whole range as if it is not splitted. Loading process should handle
    // it correctly.
    let r = cluster.get_region(b"");
    let cache_range = CacheRegion::from_region(&r);
    region_cache_engine.load_region(cache_range).unwrap();

    let product1 = ProductTable::new();
    let product2 = ProductTable::new();

    let key = product2.get_table_prefix();
    let split_key = Key::from_raw(&key).into_encoded();
    let r = cluster.get_region(&split_key);
    cluster.must_split(&r, &split_key);

    fail::cfg("ime_on_region_cache_write_batch_write_impl", "pause").unwrap();
    let handle1 = async_put(&mut cluster, &product1, 1);
    let handle2 = async_put(&mut cluster, &product2, 1);

    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        let mut tried_count = 0;
        while region_cache_engine
            .snapshot(CacheRegion::from_region(&r), u64::MAX, u64::MAX)
            .is_err()
            && tried_count < 5
        {
            std::thread::sleep(Duration::from_millis(100));
            tried_count += 1;
        }
        // Now, the range ["", "") should be cached
        let region = new_region(1, split_key, "");
        region_cache_engine.evict_region(
            &CacheRegion::from_region(&region),
            EvictReason::AutoEvict,
            None,
        );
    }

    fail::remove("ime_on_region_cache_write_batch_write_impl");
    handle1.join().unwrap();
    handle2.join().unwrap();

    for (table, is_cached) in &[(product1, true), (product2, false)] {
        fail::remove("ime_on_iterator_seek");
        let (tx, rx) = unbounded();
        fail::cfg_callback("ime_on_iterator_seek", move || {
            tx.send(true).unwrap();
        })
        .unwrap();

        must_copr_point_get(&mut cluster, table, 1);

        if *is_cached {
            rx.try_recv().unwrap();
        } else {
            rx.try_recv().unwrap_err();
        }
    }
}

#[test]
fn test_evictions_after_transfer_leader() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 2);
    cluster.run();

    let r = cluster.get_region(b"");
    cluster.must_transfer_leader(r.id, new_peer(1, 1));

    let cache_region = CacheRegion::new(1, 0, DATA_MIN_KEY, DATA_MAX_KEY);
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .core()
        .region_manager()
        .new_region(cache_region.clone());

    region_cache_engine
        .snapshot(cache_region.clone(), 100, 100)
        .unwrap();

    cluster.must_transfer_leader(r.id, new_peer(2, 2));
    region_cache_engine
        .snapshot(cache_region, 100, 100)
        .unwrap_err();
}

fn test_commit_merge(test_reload: bool) {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.run();
    let r = cluster.get_region(b"");
    cluster.must_split(&r, b"key1");

    let r = cluster.get_region(b"");
    let range1 = CacheRegion::from_region(&r);
    let r2 = cluster.get_region(b"key1");
    let range2 = CacheRegion::from_region(&r2);

    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .core()
        .region_manager()
        .new_region(range1.clone());
    region_cache_engine
        .core()
        .region_manager()
        .new_region(range2.clone());

    region_cache_engine
        .snapshot(range1.clone(), 100, 100)
        .unwrap();
    region_cache_engine
        .snapshot(range2.clone(), 100, 100)
        .unwrap();

    if !test_reload {
        // Prevent reload.
        fail::cfg("ime_on_load_region", "return").unwrap();
    }

    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.must_merge(r.get_id(), r2.get_id());

    region_cache_engine.snapshot(range1, 100, 100).unwrap_err();
    region_cache_engine.snapshot(range2, 100, 100).unwrap_err();

    if test_reload {
        // Get the latest region `r2`.
        let r2 = cluster.get_region(b"key1");
        let range2 = CacheRegion::from_region(&r2);
        // `r2` should be reloaded after merge completed.
        eventually(Duration::from_millis(100), Duration::from_secs(5), || {
            region_cache_engine
                .snapshot(range2.clone(), 100, 100)
                .is_ok()
        });
    }
}

#[test]
fn test_eviction_after_merge() {
    let test_reload = false;
    test_commit_merge(test_reload);
}

// IME should reload cached region automatically after commit merge to
// minimize the impact to coprocessor requests.
#[test]
fn test_reload_after_merge() {
    let test_reload = true;
    test_commit_merge(test_reload);
}

fn test_rollback_merge(test_reload: bool) {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 3);
    configure_for_merge(&mut cluster.cfg);
    let pd_client = Arc::clone(&cluster.pd_client);
    pd_client.disable_default_operator();

    cluster.run_conf_change();

    let region = pd_client.get_region(b"k1").unwrap();
    cluster.must_split(&region, b"k2");
    let left = pd_client.get_region(b"k1").unwrap();
    let right = pd_client.get_region(b"k2").unwrap();

    pd_client.must_add_peer(left.get_id(), new_peer(2, 2));
    pd_client.must_add_peer(right.get_id(), new_peer(2, 4));

    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");

    let mut region = pd_client.get_region(b"k1").unwrap();
    let target_region = pd_client.get_region(b"k3").unwrap();

    let schedule_merge_fp = "on_schedule_merge";
    fail::cfg(schedule_merge_fp, "return()").unwrap();

    let (tx, rx) = sync_channel(1);
    fail::cfg_callback("on_apply_res_prepare_merge", move || {
        tx.send(()).unwrap();
    })
    .unwrap();

    cluster.merge_region(region.get_id(), target_region.get_id(), Callback::None);
    // PrepareMerge is applied.
    rx.recv().unwrap();

    let leader = cluster.leader_of_region(left.get_id()).unwrap();

    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(leader.store_id);

    // After prepare merge, version becomes 2 + 1 = 3;
    region.mut_region_epoch().set_version(3);
    // load region after PrepareMerge.
    let cache_region = CacheRegion::from_region(&region);
    region_cache_engine
        .core()
        .region_manager()
        .new_region(cache_region.clone());
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(cache_region.clone(), 100, 100)
            .is_ok()
    });
    if !test_reload {
        // Prevent reload after rollback merge.
        fail::cfg("ime_on_load_region", "return").unwrap();
    }

    // Add a peer to trigger rollback.
    pd_client.must_add_peer(right.get_id(), new_peer(3, 5));
    cluster.must_put(b"k4", b"v4");
    must_get_equal(&cluster.get_engine(3), b"k4", b"v4");

    let mut region = pd_client.get_region(b"k1").unwrap();
    // After split and prepare_merge, version becomes 1 + 2 = 3;
    assert_eq!(region.get_region_epoch().get_version(), 3);
    // After ConfChange and prepare_merge, conf version becomes 1 + 2 = 3;
    assert_eq!(region.get_region_epoch().get_conf_ver(), 3);
    fail::remove(schedule_merge_fp);
    // Wait till rollback.
    cluster.must_put(b"k11", b"v11");

    // After rollback, version becomes 3 + 1 = 4;
    region.mut_region_epoch().set_version(4);
    for i in 1..3 {
        must_get_equal(&cluster.get_engine(i), b"k11", b"v11");
        let state = cluster.region_local_state(region.get_id(), i);
        assert_eq!(state.get_state(), PeerState::Normal);
        assert_eq!(*state.get_region(), region);
    }

    if !test_reload {
        // After rollback, IME cached region is evicted.
        eventually(Duration::from_millis(100), Duration::from_secs(5), || {
            let region_map = region_cache_engine
                .core()
                .region_manager()
                .regions_map()
                .read();
            region_map.regions().is_empty()
        });
    } else {
        // Get the latest region.
        let region = pd_client.get_region(b"k1").unwrap();
        let cache_region = CacheRegion::from_region(&region);
        // Region should be reloaded after merge completed.
        eventually(Duration::from_millis(100), Duration::from_secs(5), || {
            region_cache_engine
                .snapshot(cache_region.clone(), 100, 100)
                .is_ok()
        });
    }
}

// IME should also handle RollbackMerge event, we also try to evict the region
// on merge rollback for simplicity. If region is loaded after PrepareMerge and
// the merge is rollbacked, IME should track this rollback because it will also
// change epoch version.
#[test]
fn test_eviction_after_rollback_merge() {
    let test_reload = false;
    test_rollback_merge(test_reload);
}

// IME should reload cached region automatically after merge rollback to
// minimize the impact to coprocessor requests.
#[test]
fn test_reload_after_rollback_merge() {
    let test_reload = true;
    test_rollback_merge(test_reload);
}

#[test]
fn test_manual_load_range_after_transfer_leader() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 2);
    cluster.run();

    let r = cluster.get_region(b"");
    cluster.must_transfer_leader(r.id, new_peer(1, 1));

    // Set manual load range on store 2.
    let cache_range = CacheRegion::new(
        r.id,
        r.get_region_epoch().version,
        DATA_MIN_KEY.to_vec(),
        DATA_MAX_KEY.to_vec(),
    );
    let region_cache_engine = {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(2);
        region_cache_engine
            .core()
            .region_manager()
            .regions_map()
            .write()
            .add_manual_load_range(cache_range.clone());
        region_cache_engine
    };

    region_cache_engine
        .snapshot(cache_range.clone(), 100, 100)
        .unwrap_err();

    // For region in manual load range, it must load cache automatically after
    // leader transfer.
    cluster.must_transfer_leader(r.id, new_peer(2, 2));

    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(cache_range.clone(), 100, 100)
            .is_ok()
    });
}

#[test]
fn test_eviction_after_ingest_sst() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.run();

    // Generate a sst file.
    let tmp_dir = tempdir().unwrap();
    let sst_file_path = tmp_dir.path().join("test.sst");
    let mut writer = RocksSstWriterBuilder::new()
        .build(sst_file_path.to_str().unwrap())
        .unwrap();
    writer.put(&data_key(b"key"), b"value").unwrap();
    writer.finish().unwrap();

    // Add region r to cache.
    let region = cluster.get_region(b"");
    let cache_region = CacheRegion::from_region(&region);
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .core()
        .region_manager()
        .new_region(cache_region.clone());

    region_cache_engine
        .snapshot(cache_region.clone(), 100, 100)
        .unwrap();

    // Ingest the sst file.
    //
    // Build sst meta.
    let mut sst_meta = SstMeta::default();
    sst_meta.region_id = region.get_id();
    sst_meta.set_region_epoch(region.get_region_epoch().clone());
    sst_meta.set_uuid(uuid::Uuid::new_v4().as_bytes().to_vec());
    let mut content = vec![];
    let mut sst_file = File::open(&sst_file_path).unwrap();
    sst_file.read_to_end(&mut content).unwrap();
    sst_meta.crc32 = calc_crc32_bytes(&content);
    sst_meta.length = content.len() as _;
    sst_meta.cf_name = CF_DEFAULT.to_owned();

    // Prepare ingest.
    let importer = cluster.sim.rl().importers.get(&1).unwrap().clone();
    let mut f = importer.create(&sst_meta).unwrap();
    f.append(&content).unwrap();
    f.finish().unwrap();

    // Make ingest command.
    let mut ingest = Request::default();
    ingest.set_cmd_type(CmdType::IngestSst);
    ingest.mut_ingest_sst().set_sst(sst_meta.clone());
    let mut header = RaftRequestHeader::default();
    let leader = cluster.leader_of_region(region.get_id()).unwrap();
    header.set_peer(leader);
    header.set_region_id(region.get_id());
    header.set_region_epoch(region.get_region_epoch().clone());
    let mut cmd = RaftCmdRequest::default();
    cmd.set_header(header);
    cmd.mut_requests().push(ingest);
    let resp = cluster
        .call_command_on_leader(cmd, Duration::from_secs(5))
        .unwrap();
    assert!(!resp.get_header().has_error(), "{:?}", resp);

    region_cache_engine
        .snapshot(cache_region, 100, 100)
        .unwrap_err();
}

#[test]
fn test_warmup_when_transfer_leader() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 3);
    // Using a large warmup timeout to stable the test.
    cluster.cfg.raft_store.max_entry_cache_warmup_duration.0 = Duration::from_secs(1000);
    cluster.run();

    let r = cluster.get_region(b"");
    cluster.must_transfer_leader(r.id, new_peer(1, 1));
    let cache_region = CacheRegion::from_region(&r);
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .load_region(cache_region.clone())
        .unwrap();
    // put some key to trigger load
    cluster.must_put(b"k", b"val");
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(cache_region.clone(), 100, 100)
            .is_ok()
    });

    let (tx, rx) = unbounded::<()>();
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        let _ = rx.recv();
    })
    .unwrap();

    cluster.transfer_leader(r.id, new_peer(2, 2));
    // Transfer leader will not complete until the region is cached.
    sleep_ms(1000);
    cluster.reset_leader_of_region(r.id);
    let leader = cluster.leader_of_region(r.id).unwrap();
    assert_eq!(leader, new_peer(1, 1));

    // Unpause the snapshot load.
    drop(tx);
    fail::remove("ime_on_snapshot_load_finished");
    eventually(Duration::from_millis(100), Duration::from_secs(50), || {
        cluster.reset_leader_of_region(r.id);
        let leader = cluster.leader_of_region(r.id).unwrap();
        leader == new_peer(2, 2)
    });

    // The region must be loaded after transfer leader.
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(2);
    region_cache_engine
        .snapshot(cache_region.clone(), 100, 100)
        .unwrap();
}

#[test]
fn test_warmup_timeout_does_not_block_transfer_leader() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 3);
    // Using a large warmup timeout to stable the test.
    cluster.cfg.raft_store.max_entry_cache_warmup_duration.0 = Duration::from_secs(1);
    cluster.run();

    let r = cluster.get_region(b"");
    cluster.must_transfer_leader(r.id, new_peer(1, 1));
    let cache_region = CacheRegion::from_region(&r);
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .load_region(cache_region.clone())
        .unwrap();
    // put some key to trigger load
    cluster.must_put(b"k", b"val");
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(cache_region.clone(), 100, 100)
            .is_ok()
    });

    let (tx, rx) = unbounded::<()>();
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        let _ = rx.recv();
    })
    .unwrap();

    cluster.transfer_leader(r.id, new_peer(2, 2));
    // Transfer leader will not block forever when warmup times out.
    sleep(cluster.cfg.raft_store.max_entry_cache_warmup_duration.0);
    eventually(Duration::from_millis(100), Duration::from_secs(50), || {
        cluster.reset_leader_of_region(r.id);
        let leader = cluster.leader_of_region(r.id).unwrap();
        leader == new_peer(2, 2)
    });
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(2);
    region_cache_engine
        .snapshot(cache_region.clone(), 100, 100)
        .unwrap_err();

    // Unpause the snapshot load.
    drop(tx);
    fail::remove("ime_on_snapshot_load_finished");
    // put some key to trigger load
    cluster.must_put(b"k2", b"val");
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(cache_region.clone(), 100, 100)
            .is_ok()
    });
}

#[test]
fn test_background_loading_pending_region() {
    fail::cfg("ime_background_check_load_pending_interval", "return(1000)").unwrap();

    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.run();

    let r = cluster.get_region(b"");
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .load_region(CacheRegion::from_region(&r))
        .unwrap();

    let (tx, rx) = unbounded();
    fail::cfg_callback("on_apply_in_memory_engine_load_region", move || {
        tx.send(()).unwrap();
    })
    .unwrap();

    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(region_cache_engine.region_cached(&r, false));
}

// test delete range and unsafe destroy range
#[test]
fn test_delete_range() {
    let delete_range = |unsafe_destroy_range| {
        let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
        cluster.run();

        let (tx, rx) = sync_channel(0);
        fail::cfg_callback("ime_on_snapshot_load_finished", move || {
            tx.send(true).unwrap();
        })
        .unwrap();
        // load range
        {
            let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
            // Load the whole range as if it is not splitted. Loading process should handle
            // it correctly.
            let cache_range = new_region(1, "", "");
            region_cache_engine
                .load_region(CacheRegion::from_region(&cache_range))
                .unwrap();
        }

        let product = ProductTable::new();
        must_copr_load_data(&mut cluster, &product, 1);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (tx, rx) = unbounded();
        fail::cfg_callback("ime_on_iterator_seek", move || {
            tx.send(true).unwrap();
        })
        .unwrap();
        must_copr_point_get(&mut cluster, &product, 1);
        // verify it's read from in memory engine
        rx.try_recv().unwrap();

        if unsafe_destroy_range {
            let (tx, rx) = unbounded();
            let sim = cluster.sim.rl();
            let gc_worker = sim.get_gc_worker(1);
            gc_worker
                .unsafe_destroy_range(
                    Context::default(),
                    Key::from_encoded(b"".to_vec()),
                    Key::from_encoded(b"z".to_vec()),
                    Box::new(move |_| {
                        tx.send(()).unwrap();
                    }),
                )
                .unwrap();
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        } else {
            for cf in DATA_CFS {
                cluster.must_delete_range_cf(cf, b"", &[255])
            }
        }

        must_copr_point_get_empty(&mut cluster, &product, 1);
        {
            let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
            let cache_range = new_region(1, "", "");
            assert!(!region_cache_engine.region_cached(&cache_range, false));
        }
    };

    delete_range(false);
    delete_range(true);
}

#[test]
fn test_evict_on_flashback() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
    cluster.run();

    let table = ProductTable::new();

    must_copr_load_data(&mut cluster, &table, 1);
    must_copr_load_data(&mut cluster, &table, 2);
    must_copr_load_data(&mut cluster, &table, 3);

    let r = cluster.get_region(b"");
    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        region_cache_engine
            .load_region(CacheRegion::from_region(&r))
            .unwrap();
    }

    cluster.must_send_wait_flashback_msg(r.id, AdminCmdType::PrepareFlashback);
    cluster.must_send_wait_flashback_msg(r.id, AdminCmdType::FinishFlashback);

    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_on_iterator_seek", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    must_copr_point_get(&mut cluster, &table, 1);
    rx.try_recv().unwrap_err();

    must_copr_point_get(&mut cluster, &table, 2);
    rx.try_recv().unwrap_err();

    must_copr_point_get(&mut cluster, &table, 3);
    rx.try_recv().unwrap_err();
}

#[test]
fn test_load_during_flashback() {
    fail::cfg("ime_background_check_load_pending_interval", "return(1000)").unwrap();
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
    cluster.run();

    let table = ProductTable::new();

    must_copr_load_data(&mut cluster, &table, 1);
    let r = cluster.get_region(b"");
    cluster.must_send_wait_flashback_msg(r.id, AdminCmdType::PrepareFlashback);
    let (tx, rx) = unbounded();
    fail::cfg_callback("ime_fail_to_schedule_load", move || {
        tx.send(true).unwrap();
    })
    .unwrap();
    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        region_cache_engine
            .load_region(CacheRegion::from_region(&r))
            .unwrap();
    }
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        assert!(
            !region_cache_engine
                .core()
                .region_manager()
                .contains_region(r.id)
        );
    }
}

// This case test that ApplyFsm handles `Msg::Change` at the end of one round.
// This will let the `flush` call at the end flushes an empty WriteBatch, so so
// internal state of IME's write-batch may not be cleared.
#[test]
fn test_apply_prepared_but_not_write() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.cfg.raft_store.apply_batch_system.pool_size = 1;
    cluster.run();

    let r = cluster.get_region(b"k");
    cluster.must_split(&r, b"k10");

    let r1 = cluster.get_region(b"k");
    let r2 = cluster.get_region(b"k20");

    // load both regions at store 1
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .load_region(CacheRegion::from_region(&r1))
        .unwrap();
    region_cache_engine
        .load_region(CacheRegion::from_region(&r2))
        .unwrap();

    // first pause apply fsm.
    fail::cfg("before_handle_normal", "pause").unwrap();

    let (tx, rx) = unbounded();
    fail::cfg_callback("after_write_to_db_skip_write_node_1", move || {
        tx.send(()).unwrap();
    })
    .unwrap();

    // propose a write to triggers sending a fake Msg::Change.
    let req = test_raftstore::util::new_request(
        r2.id,
        r2.get_region_epoch().clone(),
        vec![new_put_cf_cmd(CF_DEFAULT, b"k20", b"v1")],
        false,
    );
    cluster
        .call_command_on_leader(req, Duration::from_millis(20))
        .unwrap_err();

    // schedule a Msg::Change to trigger a explicit commit, it will lead to a empty
    // flush at the end of this poll.
    let apply_router = cluster.get_apply_router(1).unwrap();
    apply_router.schedule_task(
        r2.id,
        ApplyTask::Change {
            cmd: ChangeObserver::from_rts(r2.id, ObserveHandle::default()),
            region_epoch: r2.get_region_epoch().clone(),
            cb: Callback::Read {
                cb: Box::new(|_| {}),
                tracker: Default::default(),
            },
        },
    );

    // resume apply fsm to let it handle raft entries and the fake `Change` msg.
    fail::remove("before_handle_normal");
    rx.recv_timeout(Duration::from_secs(1)).unwrap();

    // block apply again to batch the write of the 2 regions.
    fail::cfg("before_handle_normal", "pause").unwrap();

    // propose 2 write for 2 regions.
    let req = test_raftstore::util::new_request(
        r1.id,
        r1.get_region_epoch().clone(),
        vec![new_put_cf_cmd(CF_DEFAULT, b"k1", b"v2")],
        false,
    );
    cluster
        .call_command_on_leader(req, Duration::from_millis(10))
        .unwrap_err();
    let req = test_raftstore::util::new_request(
        r2.id,
        r2.get_region_epoch().clone(),
        vec![new_put_cf_cmd(CF_DEFAULT, b"k20", b"v2")],
        false,
    );
    cluster
        .call_command_on_leader(req, Duration::from_millis(10))
        .unwrap_err();

    // resume apply fsm, should handle new writes successfully.
    fail::remove("before_handle_normal");
    fail::remove("after_write_to_db_skip_write_node_1");

    assert_eq!(cluster.must_get(b"k1").unwrap(), b"v2");
}

#[test]
fn test_eviction_when_destroy_peer() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 1);
    cluster.run();

    let t1 = ProductTable::new();
    let t2 = ProductTable::new();

    let key = t2.get_table_prefix();
    let split_key = Key::from_raw(&key).into_encoded();
    let r = cluster.get_region(&split_key);
    cluster.must_split(&r, &split_key);
    let r = cluster.get_region(&split_key);

    let (tx, rx) = sync_channel(0);
    fail::cfg_callback("ime_on_snapshot_load_finished", move || {
        tx.send(true).unwrap();
    })
    .unwrap();
    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        let cache_region = CacheRegion::from_region(&r);
        region_cache_engine.load_region(cache_region).unwrap();
    }

    must_copr_load_data(&mut cluster, &t1, 1);
    must_copr_load_data(&mut cluster, &t2, 1);

    rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let (tx, rx) = sync_channel(0);
    fail::cfg_callback("destroy_peer", move || {
        tx.send(true).unwrap();
    })
    .unwrap();

    let router = cluster.get_router(1).unwrap();
    let mut raft_msg = RaftMessage::default();
    raft_msg.set_region_id(r.get_id());
    raft_msg.set_is_tombstone(true);
    raft_msg.set_from_peer(new_peer(0, 0));
    raft_msg.set_to_peer(r.get_peers()[0].clone());
    let mut epoch = r.get_region_epoch().clone();
    epoch.set_version(epoch.get_version() + 1);
    raft_msg.set_region_epoch(epoch);
    router.send_raft_message(raft_msg).unwrap();

    rx.recv_timeout(Duration::from_secs(2)).unwrap();

    {
        let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
        assert!(!region_cache_engine.region_cached(&r, false));
    }
}

// IME must not panic when destroy an uninitialized region.
#[test]
fn test_eviction_when_destroy_uninitialized_peer() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 2);
    let pd_client = cluster.pd_client.clone();
    pd_client.disable_default_operator();
    cluster.run_conf_change();

    let region = pd_client.get_region(b"").unwrap();
    assert!(
        !region.get_peers().iter().any(|p| p.get_store_id() == 2),
        "{:?}",
        region
    );

    // Block snapshot messages, so that new peers will never be initialized.
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(region.get_id(), 2)
            .msg_type(MessageType::MsgSnapshot)
            .direction(Direction::Recv),
    ));

    let learner1 = new_learner_peer(2, 2);
    pd_client.must_add_peer(region.get_id(), learner1.clone());
    cluster.must_region_exist(region.get_id(), 2);
    pd_client.must_remove_peer(region.get_id(), learner1);

    // IME observes all peer destroy events to timely evict regions. By adding a
    // new peer, the old and uninitialized peer will be destroyed and IME must
    // not panic in this case.
    let learner2 = new_learner_peer(2, 3);
    pd_client.must_add_peer(region.get_id(), learner2.clone());
    cluster.must_region_exist(region.get_id(), 2);
}

// IME must not panic when warmup an uninitialized peer.
#[test]
fn test_transfer_leader_warmup_uninitialized_peer() {
    let mut cluster = new_server_cluster_with_hybrid_engine(0, 2);
    let pd_client = cluster.pd_client.clone();
    pd_client.disable_default_operator();
    cluster.run_conf_change();

    let region = pd_client.get_region(b"").unwrap();
    assert!(
        !region.get_peers().iter().any(|p| p.get_store_id() == 2),
        "{:?}",
        region
    );

    // Load the region in leader.
    let region_cache_engine = cluster.sim.rl().get_region_cache_engine(1);
    region_cache_engine
        .load_region(CacheRegion::from_region(&region))
        .unwrap();
    // Put some key to trigger load
    cluster.must_put(b"k", b"val");
    eventually(Duration::from_millis(100), Duration::from_secs(5), || {
        region_cache_engine
            .snapshot(CacheRegion::from_region(&region), 100, 100)
            .is_ok()
    });

    // Block snapshot messages, so that new peers will never be initialized.
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(region.get_id(), 2)
            .msg_type(MessageType::MsgSnapshot)
            .direction(Direction::Recv),
    ));

    let peer1 = new_peer(2, 2);
    pd_client.must_add_peer(region.get_id(), peer1.clone());
    cluster.must_region_exist(region.get_id(), 2);

    // IME will send a MsgPreLoadRegion message before transferring leader,
    // and this message must not cause panic.
    cluster.transfer_leader(region.get_id(), new_peer(2, 2));
    // Give some time for handling MsgPreLoadRegion message.
    sleep_ms(100);
    cluster.clear_send_filters();
    cluster.must_transfer_leader(region.get_id(), new_peer(2, 2));
}
