import type { ReactNode } from "react";

/**
 * 设置每一节的标题：吸顶，内容从它下面滚过去；铺满左右内边距，滚过去的行不从两侧露出来。
 * 关闭按钮在弹窗顶栏里，这里不用让位。`right` 放这一节的主控件（如默认模型下拉）。
 */
export function SectionHead({ title, description, right }: { title: string; description?: ReactNode; right?: ReactNode }) {
  return (
    <div className="sticky top-0 z-1 -mx-(--pad) mb-1 flex items-center gap-4 bg-surface px-(--pad) pt-6 pb-3 max-[900px]:pt-5 max-[600px]:flex-col max-[600px]:items-start max-[600px]:pt-4 max-[600px]:pb-2.5">
      <div className="min-w-0 flex-1">
        <h3 className="text-section-title font-semibold">{title}</h3>
        {description && <p className="mt-1 text-label text-muted">{description}</p>}
      </div>
      {right}
    </div>
  );
}
