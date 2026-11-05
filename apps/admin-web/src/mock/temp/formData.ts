// 高级表格演示页（views/examples/tables/index.vue）的静态演示数据。
// 该页的 dataTransformer 只读取 avatar 字段，其余列由页面内随机生成。
import avatar1 from '@/assets/images/avatar/avatar1.webp'
import avatar2 from '@/assets/images/avatar/avatar2.webp'
import avatar3 from '@/assets/images/avatar/avatar3.webp'
import avatar4 from '@/assets/images/avatar/avatar4.webp'
import avatar5 from '@/assets/images/avatar/avatar5.webp'
import avatar6 from '@/assets/images/avatar/avatar6.webp'
import avatar7 from '@/assets/images/avatar/avatar7.webp'
import avatar8 from '@/assets/images/avatar/avatar8.webp'

export interface AccountTableMockItem {
  avatar: string
}

export const ACCOUNT_TABLE_DATA: AccountTableMockItem[] = [
  { avatar: avatar1 },
  { avatar: avatar2 },
  { avatar: avatar3 },
  { avatar: avatar4 },
  { avatar: avatar5 },
  { avatar: avatar6 },
  { avatar: avatar7 },
  { avatar: avatar8 }
]
