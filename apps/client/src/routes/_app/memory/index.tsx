import { createFileRoute } from "@tanstack/react-router";
import { MemoryPage } from "@/features/memory/memory-page";

export const Route = createFileRoute("/_app/memory/")({ component: () => <MemoryPage selected={null} /> });
