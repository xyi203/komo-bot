import { addMatter, keys } from "@komo/api";
import type { MatterAddRequest, MatterKind } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";
import { FormError, Input, Label, Radio } from "./parts";

/** 新建事项（docs/web.md §12）：目标可带下一步与截止日期，领域可带标准；不带会话，是终端那一侧。 */
export function NewMatterForm({
  compact,
  onBack,
  onClose,
  onCreated,
}: {
  compact: boolean;
  onBack: () => void;
  onClose: () => void;
  onCreated: (id: string) => void;
}) {
  const queryClient = useQueryClient();
  const [kind, setKind] = useState<MatterKind>("goal");
  const [busy, setBusy] = useState(false);
  const [fail, setFail] = useState("");

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const v = (key: string) => String(data.get(key) ?? "").trim();
    const body: MatterAddRequest = { kind, title: v("title"), request_key: `web:matter-add:${crypto.randomUUID()}` };
    if (kind === "goal") {
      if (v("next")) body.next = v("next");
      if (v("due")) body.due = v("due");
    } else if (v("standard")) body.standard = v("standard");
    setBusy(true);
    setFail("");
    try {
      const response = await addMatter(body);
      await queryClient.invalidateQueries({ queryKey: keys.matters });
      onCreated(response.matter.id);
    } catch (error) {
      setFail(m.matter_error_create({ message: errorText(error) }));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={(event) => void submit(event)} className={compact ? "pt-1" : "px-4.5 pt-4 pb-4.5"}>
      <div className="flex min-h-7 items-center gap-2">
        {compact && <IconButton name="left" label={m.matter_back()} onClick={onBack} className="-ml-2" />}
        <h2 className="flex-1 text-section-title font-semibold">{m.matter_new_title()}</h2>
        {!compact && <IconButton name="x" label={m.close()} onClick={onClose} className="-mr-2" />}
      </div>
      <fieldset className="mt-3.5 border-0 p-0">
        <legend className="mb-1.5 p-0 text-label font-medium">{m.matter_new_what()}</legend>
        <Radio name="kind" value="goal" checked={kind === "goal"} onChange={() => setKind("goal")}>
          {m.matter_new_goal()}
        </Radio>
        <Radio name="kind" value="area" checked={kind === "area"} onChange={() => setKind("area")}>
          {m.matter_new_area()}
        </Radio>
      </fieldset>
      <Label htmlFor="mn-title">{m.matter_new_one_line()}</Label>
      <Input id="mn-title" name="title" required maxLength={80} placeholder={m.matter_new_placeholder()} />
      {kind === "goal" ? (
        <>
          <Label htmlFor="mn-next">{m.matter_new_next()}</Label>
          <Input id="mn-next" name="next" maxLength={300} />
          <Label htmlFor="mn-due">{m.matter_new_due()}</Label>
          <Input id="mn-due" name="due" type="date" />
        </>
      ) : (
        <>
          <Label htmlFor="mn-standard">{m.matter_new_standard()}</Label>
          <Input id="mn-standard" name="standard" maxLength={300} placeholder={m.matter_new_standard_placeholder()} />
        </>
      )}
      {fail && <FormError>{fail}</FormError>}
      <div className="mt-4 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? m.matter_new_busy() : m.matter_new_submit()}
        </Button>
        <Button onClick={onClose}>{m.cancel()}</Button>
      </div>
    </form>
  );
}
