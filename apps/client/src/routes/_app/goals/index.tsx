import { createFileRoute } from "@tanstack/react-router";
import { GoalsPage } from "@/features/goals/goals-page";

export const Route = createFileRoute("/_app/goals/")({ component: GoalsPage });
