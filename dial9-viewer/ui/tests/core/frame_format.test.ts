import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

// Synthetic symbols preserve the nested Rust shapes seen in production without
// copying private crate, service, or type names into the repository.
const { formatFrame } = require("../../trace_parser.js") as {
  formatFrame: (frame: { symbol: string; location: string | null }) => {
    text: string;
    docsUrl: string | null;
  };
};
const {
  buildFlamegraphTree,
  buildFlamegraphTreeFromApi,
} = require("../../trace_analysis.js") as {
  buildFlamegraphTree: (
    samples: { callchain: string[] }[],
    callframeSymbols: Map<
      string,
      { symbol: string; location: string | null }
    >,
  ) => {
    children: Map<string, {
      name: string;
      fullName?: string;
    }>;
  };
  buildFlamegraphTreeFromApi: (root: {
    name: string;
    count: number;
    self: number;
    children?: {
      name: string;
      count: number;
      self: number;
    }[];
  }, frameCache?: Map<string, {
    key: string;
    text: string;
    location: string | null;
    docsUrl: string | null;
  }>) => {
    name: string;
    fullName?: string;
    count: number;
    self: number;
    children: Map<string, {
      name: string;
      fullName?: string;
      count: number;
      self: number;
    }>;
  };
};

const taskLocalTryNext =
  "<tokio::task::task_local::TaskLocalFuture<example_context::RequestContext, example_stream::stream_ext::try_next::TryNext<core::pin::Pin<alloc::boxed::Box<dyn futures_core::stream::Stream<Item = core::result::Result<example_types::verified_item::VerifiedItem<example_types::hash_state::HashState>, alloc::boxed::Box<dyn core::error::Error + core::marker::Sync + core::marker::Send>>> + core::marker::Send>>>> as core::future::future::Future>::poll";

function format(symbol: string): string {
  return formatFrame({ symbol, location: null }).text;
}

describe("Rust flamegraph frame formatting", () => {
  it("surfaces the transform behind an AsyncStream poll", () => {
    expect(
      format(
        "<async_stream::async_stream::AsyncStream<core::result::Result<T, E>, <example_pipeline::transform_stream::TransformStream<core::pin::Pin<S>, example_pipeline::stages::checksum::ChecksumStage<T>>>::into_stream::{closure#0}> as futures_core::stream::Stream>::poll_next",
      ),
    ).toBe("checksum::ChecksumStage · TransformStream::into_stream");
  });

  it("skips nested closure suffixes to retain the owning async function", () => {
    expect(
      format(
        "<core::future::poll_fn::PollFn<<example_store::fetch::FetchRecord<A, B, example_cache::dispatcher::CacheDispatch>>::next_chunk::{closure#0}::{closure#0}> as core::future::future::Future>::poll",
      ),
    ).toBe("dispatcher::CacheDispatch · FetchRecord::next_chunk");
  });

  it("keeps the transform module and concrete payload visible", () => {
    expect(
      format(
        "<async_stream::async_stream::AsyncStream<T, <example_pipeline::transform_stream::TransformStream<S, example_pipeline::stages::chunker::Chunker<example_types::record_batch::RecordBatch>>>::into_stream::{closure#0}> as futures_core::stream::Stream>::poll_next",
      ),
    ).toBe("chunker::Chunker<RecordBatch> · TransformStream::into_stream");
  });

  it("uses the closure owner directly when the implementing type is a closure", () => {
    expect(
      format(
        "<example_service::handler::serve::{closure#0} as core::future::future::Future>::poll",
      ),
    ).toBe("handler::serve");
  });

  it("uses the outer implementing type instead of a nested generic tail", () => {
    expect(
      format(
        "<tokio::runtime::task::core::Core<MyFuture, alloc::sync::Arc<tokio::runtime::scheduler::multi_thread::handle::Handle>> as core::future::future::Future>::poll",
      ),
    ).toBe("MyFuture");
  });

  it("formats qualified inherent methods that omit an as-Trait clause", () => {
    expect(
      format(
        "<tokio::runtime::task::harness::Harness<tokio::runtime::blocking::task::BlockingTask<<tokio::fs::file::Inner>::spawn_blocking_read::{closure#0}>, tokio::runtime::blocking::schedule::BlockingSchedule>>::poll",
      ),
    ).toBe("Inner::spawn_blocking_read");
  });

  it("uses the payload type when a qualified wrapper has no closure", () => {
    expect(
      format(
        "<tokio::runtime::task::core::Core<hyper::client::conn::Connection<I, B>, alloc::sync::Arc<tokio::runtime::scheduler::current_thread::Handle>>>::poll",
      ),
    ).toBe("conn::Connection");
  });

  it("preserves the meaningful item type nested inside a stream adapter", () => {
    expect(format(taskLocalTryNext)).toBe(
      "try_next::TryNext<VerifiedItem<HashState>>",
    );
  });

  it("preserves concise non-async trait implementations", () => {
    expect(
      format(
        "<example_checksums::crc::native::Crc32c as example_checksums::Checksummer>::update",
      ),
    ).toBe("Crc32c::update");
  });

  it("recognizes legacy rustc closure spelling", () => {
    expect(
      format(
        "<wrapper::FutureWrap<service::Worker::run::{{closure}}> as core::future::Future>::poll",
      ),
    ).toBe("Worker::run");
  });

  it("uses the same labels for aggregate trees without merging collisions", () => {
    const firstCrc =
      "<example_a::Crc32c as example_checksums::Checksummer>::update";
    const secondCrc =
      "<example_b::Crc32c as example_checksums::Checksummer>::update";
    const apiRoot = {
      name: "(all)",
      count: 3,
      self: 0,
      children: [
        { name: taskLocalTryNext, count: 1, self: 1 },
        { name: firstCrc, count: 1, self: 1 },
        { name: secondCrc, count: 1, self: 1 },
      ],
    };
    const frameCache = new Map();
    const tree = buildFlamegraphTreeFromApi(apiRoot, frameCache);
    const exactTree = buildFlamegraphTree(
      [{ callchain: ["0x1"] }],
      new Map([
        ["0x1", { symbol: taskLocalTryNext, location: null }],
      ]),
    );

    expect(tree.children.get(taskLocalTryNext)).toMatchObject({
      name: "try_next::TryNext<VerifiedItem<HashState>>",
      fullName: taskLocalTryNext,
    });
    const exactNode = exactTree.children.get(taskLocalTryNext);
    expect(tree.children.get(taskLocalTryNext)).toMatchObject({
      name: exactNode?.name,
      fullName: exactNode?.fullName,
    });
    expect(tree.children.get(firstCrc)?.name).toBe("Crc32c::update");
    expect(tree.children.get(secondCrc)?.name).toBe("Crc32c::update");
    expect(tree.children.size).toBe(3);
    expect(frameCache.size).toBe(4);

    buildFlamegraphTreeFromApi(apiRoot, frameCache);
    expect(frameCache.size).toBe(4);
  });
});
