import { useCallback, useMemo, useState } from "react";

/**
 * 轨迹页的界面状态：搜索词、概览是等宽还是按时长、概览上拖出的区间、选中的记录与详情开在哪一页、收起的轮次与调用。
 * 提到会话页这一层，切到别的标签再回来不丢（原型里也是这样：只在换会话时清空）。
 */
export interface TraceUi {
  query: string;
  setQuery: (query: string) => void;
  byDuration: boolean;
  toggleDuration: () => void;
  /** 概览定义域里的 [起, 止]。 */
  range: [number, number] | null;
  setRange: (range: [number, number] | null) => void;
  selected: string | null;
  /** `jumped`：从对话里的"查看"或详情里的跳转来的，详情回到"概述"页。 */
  select: (id: string | null, jumped?: boolean) => void;
  detailTab: string;
  setDetailTab: (tab: string) => void;
  turnsClosed: ReadonlySet<number>;
  setTurnsClosed: (next: ReadonlySet<number>) => void;
  toggleTurn: (n: number) => void;
  callsClosed: ReadonlySet<string>;
  setCallsClosed: (next: ReadonlySet<string>) => void;
  toggleCall: (id: string) => void;
}

export function useTraceUi(): TraceUi {
  const [query, setQuery] = useState("");
  const [byDuration, setByDuration] = useState(false);
  const [range, setRange] = useState<[number, number] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [detailTab, setDetailTab] = useState("overview");
  const [turnsClosed, setTurnsClosed] = useState<ReadonlySet<number>>(new Set());
  const [callsClosed, setCallsClosed] = useState<ReadonlySet<string>>(new Set());

  const toggleDuration = useCallback(() => {
    setByDuration((v) => !v);
    setRange(null);
  }, []);
  const select = useCallback((id: string | null, jumped = false) => {
    setSelected(id);
    if (jumped) setDetailTab("overview");
  }, []);
  const toggleTurn = useCallback((n: number) => {
    setTurnsClosed((previous) => {
      const next = new Set(previous);
      if (!next.delete(n)) next.add(n);
      return next;
    });
  }, []);
  const toggleCall = useCallback((id: string) => {
    setCallsClosed((previous) => {
      const next = new Set(previous);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  }, []);

  return useMemo(
    () => ({
      query, setQuery, byDuration, toggleDuration, range, setRange, selected, select, detailTab, setDetailTab,
      turnsClosed, setTurnsClosed, toggleTurn, callsClosed, setCallsClosed, toggleCall,
    }),
    [query, byDuration, toggleDuration, range, selected, select, detailTab, turnsClosed, toggleTurn, callsClosed, toggleCall],
  );
}
