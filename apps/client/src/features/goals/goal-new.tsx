import { addMatter, keys, uuid } from "@komo/api";
import type { MatterAddRequest } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { IconButton } from "@/components/ui/icon-button";
import { m } from "@/paraglide/messages.js";
import { serverText } from "./model";
import { FormError, Hint, Input, Label, Textarea } from "./parts";

/**
 * 新建目标的弹窗：标题、说明、是否没有终点（Gateway 里是「领域」一类）；有终点的可以再给截止日期。
 * 建好直接进它的详情页，定时、绑定支线都在那里加。
 */
export function GoalNewDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby={undefined} className="w-[min(480px,calc(100vw-32px))]">
        <NewForm onClose={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

function NewForm({ onClose }: { onClose: () => void }) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [ongoing, setOngoing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [fail, setFail] = useState("");

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const v = (key: string) => String(data.get(key) ?? "").trim();
    const body: MatterAddRequest = { kind: ongoing ? "area" : "goal", title: v("title"), request_key: `web:matter-add:${uuid()}` };
    if (v("description")) body.description = v("description");
    if (!ongoing && v("due")) body.due = v("due");
    setBusy(true);
    setFail("");
    try {
      const response = await addMatter(body);
      await queryClient.invalidateQueries({ queryKey: keys.matters });
      onClose();
      void navigate({ to: "/goals/$id", params: { id: response.matter.id } });
    } catch (error) {
      setFail(m.matter_error_create({ message: serverText(error) }));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={(event) => void submit(event)} className="p-5">
      <div className="flex min-h-8 items-center gap-2">
        <DialogTitle className="m-0 flex-1 text-section-title font-semibold">{m.goal_new_title()}</DialogTitle>
        <DialogClose asChild>
          <IconButton name="x" label={m.close()} className="-mr-2" />
        </DialogClose>
      </div>
      <Label htmlFor="gn-title">{m.matter_new_one_line()}</Label>
      <Input id="gn-title" name="title" required maxLength={80} autoFocus placeholder={m.goal_new_placeholder()} />
      <Label htmlFor="gn-desc">{m.goal_new_description()}</Label>
      <Textarea id="gn-desc" name="description" maxLength={8000} placeholder={m.goal_new_description_hint()} />
      <label className="mt-3.5 flex cursor-pointer items-center gap-2 text-label">
        <input type="checkbox" className="m-0 accent-primary" checked={ongoing} onChange={(event) => setOngoing(event.target.checked)} />
        {m.goal_new_ongoing()}
      </label>
      <Hint className="mt-1 ml-5.5">{m.goal_new_ongoing_hint()}</Hint>
      {!ongoing && (
        <>
          <Label htmlFor="gn-due">{m.matter_new_due()}</Label>
          <Input id="gn-due" name="due" type="date" />
        </>
      )}
      {fail && <FormError>{fail}</FormError>}
      <div className="mt-5 flex flex-wrap gap-2">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? m.matter_new_busy() : m.matter_new_submit()}
        </Button>
        <Button onClick={onClose}>{m.cancel()}</Button>
      </div>
    </form>
  );
}
