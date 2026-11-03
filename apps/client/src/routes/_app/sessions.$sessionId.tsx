import { createFileRoute } from "@tanstack/react-router";
import { SessionPage } from "@/features/session/session-page";

export const Route = createFileRoute("/_app/sessions/$sessionId")({ component: Page });

function Page() {
  const { sessionId } = Route.useParams();
  return <SessionPage sessionId={sessionId} />;
}
