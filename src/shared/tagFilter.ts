/** 标签筛选范围：当前视图保留全部条件，其余范围只保留对应的文件夹/集合/全库。 */
export type TagFilterScope = "view" | "folder" | "collection" | "library";

/** apply = 用这个标签替换标签条件；append = 在当前视图里叠加；remove = 取消该标签。 */
export type TagFilterAction = "apply" | "append" | "remove";
