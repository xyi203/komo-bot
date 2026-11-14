import { createFileRoute, redirect } from "@tanstack/react-router";

export const Route = createFileRoute("/_app/matters/$id")({
  beforeLoad: ({ params }) => {
    throw redirect({ to: "/goals/$id", params: { id: params.id }, replace: true });
  },
});
