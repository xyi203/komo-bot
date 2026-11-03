import { createFileRoute } from "@tanstack/react-router";
import { InboxPage } from "@/features/inbox/inbox-page";

export const Route = createFileRoute("/_app/inbox/")({ component: () => <InboxPage selected={null} /> });
