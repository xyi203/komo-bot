import * as Primitive from "@radix-ui/react-tabs";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

// shadcn/ui 的 Tabs 骨架，只带 Radix 的行为（方向键切换、tablist 语义）；外观按用处各自传 className：
// 对话 / 轨迹 / 改动是灰药丸，轨迹详情是底线标签。
const Tabs = Primitive.Root;

function TabsList({ className, ...props }: ComponentProps<typeof Primitive.List>) {
  return <Primitive.List className={cn("inline-flex", className)} {...props} />;
}

function TabsTrigger({ className, ...props }: ComponentProps<typeof Primitive.Trigger>) {
  return <Primitive.Trigger className={cn("cursor-pointer whitespace-nowrap", className)} {...props} />;
}

const TabsContent = Primitive.Content;

export { Tabs, TabsContent, TabsList, TabsTrigger };
