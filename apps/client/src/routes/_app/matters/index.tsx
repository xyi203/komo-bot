import { createFileRoute } from "@tanstack/react-router";
import { MattersPage } from "@/features/matters/matters-page";

export const Route = createFileRoute("/_app/matters/")({ component: () => <MattersPage selected={null} /> });
