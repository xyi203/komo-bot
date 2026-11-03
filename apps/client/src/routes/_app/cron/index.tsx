import { createFileRoute } from "@tanstack/react-router";
import { CronPage } from "@/features/cron/cron-page";

export const Route = createFileRoute("/_app/cron/")({ component: () => <CronPage selected={null} /> });
