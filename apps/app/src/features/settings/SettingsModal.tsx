import { useConnection } from "@/shared/api/use-connection";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/shared/ui/dialog";
import { EmptyState } from "@/shared/ui/empty-state";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/shared/ui/tabs";
import { GeneralTab } from "./tabs/general";
import { RunsTab } from "./tabs/runs";

// Memories left this modal for a surface of their own (features/memory) — they
// are the product, not a setting.
const TABS = [
  { value: "general", label: "常规", Panel: GeneralTab, needsGateway: false },
  { value: "runs", label: "运行", Panel: RunsTab, needsGateway: true },
] as const;

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const { connected } = useConnection();

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="flex h-[620px] max-h-[85vh] flex-col gap-0 overflow-hidden p-0 sm:max-w-[680px]">
        <DialogHeader className="border-b border-border px-6 py-5">
          <DialogTitle className="text-base font-semibold tracking-tight">设置</DialogTitle>
        </DialogHeader>

        <Tabs defaultValue="general" className="mt-3 flex min-h-0 flex-col gap-0">
          <TabsList
            variant="line"
            className="h-auto w-full justify-start rounded-none border-b border-border px-6"
          >
            {TABS.map((tab) => (
              <TabsTrigger key={tab.value} value={tab.value} className="flex-none">
                {tab.label}
              </TabsTrigger>
            ))}
          </TabsList>

          <div className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
            {TABS.map(({ value, Panel, needsGateway }) => (
              <TabsContent key={value} value={value}>
                {needsGateway && !connected ? (
                  <EmptyState>未连接到 gateway。</EmptyState>
                ) : (
                  <Panel />
                )}
              </TabsContent>
            ))}
          </div>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
