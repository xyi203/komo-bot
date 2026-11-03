import type { ReactNode } from "react";

/**
 * 设置每一节的标题：吸顶，内容从它下面滚过去；铺满左右内边距，否则卡片的环会从两侧 1px 露出来。
 * 右上角的关闭按钮落在它上面（右内边距让出 40px），不压内容。`right` 放这一节的主控件（如默认模型下拉）。
 */
export function SectionHead({ title, description, right }: { title: string; description?: ReactNode; right?: ReactNode }) {
  return (
    <div className="sticky top-0 z-1 -mx-(--pad) mb-1 flex items-end gap-4 bg-surface pt-6 pr-[calc(var(--pad)+40px)] pb-3 pl-(--pad) max-[900px]:pt-5 max-[600px]:flex-col max-[600px]:items-start max-[600px]:pt-4 max-[600px]:pr-(--pad) max-[600px]:pb-2.5">
      <div className="min-w-0 flex-1">
        <h3 className="text-dialog-title font-semibold">{title}</h3>
        {description && <p className="mt-1 text-label text-muted">{description}</p>}
      </div>
      {right}
    </div>
  );
}
