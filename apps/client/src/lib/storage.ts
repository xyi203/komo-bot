import { useCallback, useState } from "react";

// 偏好只存在这个浏览器的 localStorage（与原型同一组键，`komo.*`）。读写都可能抛（隐私模式、配额），
// 失败就当没存过：界面照常工作，只是不记住。

export function readStored<T>(key: string, parse: (raw: unknown) => T | undefined, fallback: T): T {
  if (typeof window === "undefined") return fallback;
  try {
    const text = window.localStorage.getItem(key);
    if (text === null) return fallback;
    let raw: unknown = text;
    try {
      raw = JSON.parse(text);
    } catch {
      // 不是 JSON：原型里 group / sort / filter 存的是裸字符串
    }
    return parse(raw) ?? fallback;
  } catch {
    return fallback;
  }
}

export function writeStored(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // 不记住而已
  }
}

/** 取值受限的字符串偏好（`group` / `sort` / `filter`）。 */
export function oneOf<T extends string>(allowed: readonly T[]): (raw: unknown) => T | undefined {
  return (raw) => (typeof raw === "string" && (allowed as readonly string[]).includes(raw) ? (raw as T) : undefined);
}

/** 字符串集合偏好（收起的分组、展开了子任务的父会话），JSON 数组。 */
export function usePersistedSet(key: string) {
  const [set, setSet] = useState<ReadonlySet<string>>(
    () => new Set(readStored<string[]>(key, (raw) => (Array.isArray(raw) ? raw.filter((x) => typeof x === "string") : undefined), [])),
  );
  const toggle = useCallback(
    (id: string, force?: boolean) => {
      setSet((previous) => {
        const next = new Set(previous);
        const want = force ?? !next.has(id);
        if (want) next.add(id);
        else next.delete(id);
        writeStored(key, JSON.stringify([...next]));
        return next;
      });
    },
    [key],
  );
  return [set, toggle] as const;
}

/** 单个字符串偏好。 */
export function usePersistedChoice<T extends string>(key: string, allowed: readonly T[], fallback: T) {
  const [value, setValue] = useState<T>(() => readStored(key, oneOf(allowed), fallback));
  const update = useCallback(
    (next: T) => {
      setValue(next);
      writeStored(key, next);
    },
    [key],
  );
  return [value, update] as const;
}
