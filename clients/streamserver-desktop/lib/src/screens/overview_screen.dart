import 'package:flutter/material.dart';

import '../utils.dart';
import '../widgets/data_panel.dart';

class OverviewScreen extends StatelessWidget {
  const OverviewScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '系统总览',
          description: '在一页里查看任务、在线流、录像、文件产物和节点健康概况。',
        ),
        AsyncDataPanel(
          loader: (controller) async {
            final tasks = await controller.api('GET', '/api/v1/tasks', query: {'page_size': 8});
            final streams = await controller.api('GET', '/api/v1/streams');
            final records = await controller.api('GET', '/api/v1/records', query: {'page_size': 1});
            final artifacts = await controller.api('GET', '/api/v1/file-artifacts', query: {'page_size': 1});
            final nodes = await controller.api('GET', '/api/v1/nodes');
            return {
              'tasks': tasks,
              'streams': streams['value'],
              'records': records,
              'artifacts': artifacts,
              'nodes': nodes['value'],
            };
          },
          builder: (context, data) {
            final map = (data as Map).cast<String, Object?>();
            final taskPage = (map['tasks'] as Map).cast<String, Object?>();
            final streams = rowsFrom(map['streams']);
            final nodes = rowsFrom(map['nodes']);
            final records = (map['records'] as Map).cast<String, Object?>();
            final artifacts = (map['artifacts'] as Map).cast<String, Object?>();
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                KeyValueGrid(
                  items: {
                    '任务总数': taskPage['total'],
                    '在线流': streams.length,
                    '录像记录': records['total'],
                    '文件产物': artifacts['total'],
                    '在线节点': nodes.where((node) => node['healthy'] == true).length,
                  },
                ),
                const SizedBox(height: 18),
                Surface(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      const Text('最近任务', style: TextStyle(fontWeight: FontWeight.w700)),
                      const SizedBox(height: 12),
                      _RecentTasksTable(rowsFrom(taskPage['items'])),
                    ],
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }
}

class _RecentTasksTable extends StatelessWidget {
  const _RecentTasksTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return SizedBox(
      width: double.infinity,
      child: DataTable(
        columns: const [
          DataColumn(label: Text('任务')),
          DataColumn(label: Text('类型')),
          DataColumn(label: Text('状态')),
          DataColumn(label: Text('创建时间')),
        ],
        rows: rows.map((row) {
          return DataRow(
            cells: [
              DataCell(Text(textValue(row['name']))),
              DataCell(Text(textValue(row['type']))),
              DataCell(Text(textValue(row['status']))),
              DataCell(Text(textValue(row['created_at']))),
            ],
          );
        }).toList(),
      ),
    );
  }
}
