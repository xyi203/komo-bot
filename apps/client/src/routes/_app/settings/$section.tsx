import { createFileRoute } from "@tanstack/react-router";
import { SettingsEntry } from "@/features/settings/settings-entry";

export const Route = createFileRoute("/_app/settings/$section")({
  component: function Page() {
    return <SettingsEntry section={Route.useParams().section} />;
  },
});
