import { rebuildMemoryIndex, useMemoryIndexStatus } from "@komo/api";
import type { RebuildIndexResponse } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { BlockError, SkeletonRows } from "@/components/page";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { formatNumber } from "@/lib/format";
import { m } from "@/paraglide/messages.js";
import { KeyValues, Notice, Panel } from "../controls";
import { SectionHead } from "../section-head";

const stateText = (state: string) =>
  state === "ready" ? m.memindex_state_ready() : state === "building" ? m.memindex_state_building() : state === "failed" ? m.memindex_state_failed() : state === "unconfigured" ? m.memindex_state_unconfigured() : state;

/** 记忆索引：记忆检索的向量部分；索引不可用时检索按关键词降级。 */
export function IndexSection() {
  const status = useMemoryIndexStatus();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<RebuildIndexResponse | { error: string } | null>(null);

  const head = <SectionHead title={m.settings_sec_index()} description={m.memindex_desc()} />;
  if (status.isError) {
    return (
      <>
        {head}
        <BlockError message={m.memindex_load_error({ message: errorText(status.error) })} onRetry={() => void status.refetch()} />
      </>
    );
  }
  const ix = status.data;
  if (!ix) {
    return (
      <>
        {head}
        <SkeletonRows count={3} />
      </>
    );
  }
  const rebuild = async () => {
    setBusy(true);
    try {
      setResult(await rebuildMemoryIndex());
      await queryClient.invalidateQueries({ queryKey: ["config", "memory-index"] });
    } catch (error) {
      setResult({ error: errorText(error) });
    }
    setBusy(false);
  };
  const errors = ix.errors ?? [];
  return (
    <>
      {head}
      <Panel>
        <KeyValues
          rows={[
            [m.memindex_vector(), ix.space ? `${ix.space.model} · ${ix.space.provider}` : <span className="text-muted">{m.memindex_none()}</span>],
            ix.space ? [m.memindex_endpoint(), <span className="font-mono">{ix.space.endpoint}</span>] : null,
            [
              m.memindex_state(),
              <>
                {stateText(ix.state)}
                {ix.state !== "unconfigured" && <> · {m.memindex_covered({ indexed: formatNumber(Number(ix.indexed)), total: formatNumber(Number(ix.total)) })}</>}
              </>,
            ],
            errors.length ? [m.memindex_errors(), <span className="text-negative">{errors.join("; ")}</span>] : null,
          ]}
        />
        {result && (
          <Notice>
            {"error" in result ? (
              <p className="text-negative">{m.memindex_rebuild_failed({ message: result.error })}</p>
            ) : (
              <p>{result.accepted ? m.memindex_rebuild_accepted({ generation: result.generation }) : m.memindex_rebuild_running()}</p>
            )}
          </Notice>
        )}
        <div className="mt-3">
          <Button disabled={busy} onClick={() => void rebuild()}>
            {busy ? m.memindex_rebuilding() : m.memindex_rebuild()}
          </Button>
        </div>
      </Panel>
    </>
  );
}
