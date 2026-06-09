import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class MediaUploadScreen extends StatefulWidget {
  const MediaUploadScreen({super.key});

  @override
  State<MediaUploadScreen> createState() => _MediaUploadScreenState();
}

class _MediaUploadScreenState extends State<MediaUploadScreen> {
  final fileController = TextEditingController();
  final keywordController = TextEditingController();
  final nodeController = TextEditingController();
  String statusFilter = 'active';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;
  String? result;
  bool uploading = false;

  @override
  void dispose() {
    fileController.dispose();
    keywordController.dispose();
    nodeController.dispose();
    super.dispose();
  }

  void _refresh({bool resetPage = false}) {
    setState(() {
      if (resetPage) page = 1;
      refreshSeed++;
    });
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '媒资上传',
          description:
              '通过 Rust native multipart 客户端上传本地文件到 StreamServer Agent 节点。',
        ),
        Surface(
          child: Row(
            children: [
              Expanded(
                child: TextField(
                  controller: fileController,
                  decoration: const InputDecoration(labelText: '本地文件路径'),
                ),
              ),
              const SizedBox(width: 12),
              FilledButton.icon(
                onPressed: uploading ? null : () => _upload(controller),
                icon: uploading
                    ? const SizedBox(
                        width: 18,
                        height: 18,
                        child: CircularProgressIndicator(strokeWidth: 2))
                    : const Icon(Icons.upload_file),
                label: Text(uploading ? '上传中' : '上传'),
              ),
            ],
          ),
        ),
        if (result != null) ...[
          const SizedBox(height: 12),
          Surface(child: SelectableText(result!)),
        ],
        const SizedBox(height: 12),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              keywordController.clear();
              nodeController.clear();
              statusFilter = 'active';
              _refresh(resetPage: true);
            },
            children: [
              SmallSelect(
                label: '状态',
                value: statusFilter,
                options: const ['active', 'deleted', 'all'],
                onChanged: (value) => setState(() => statusFilter = value),
              ),
              SmallTextField(
                  controller: keywordController,
                  label: '关键字',
                  onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(
                  controller: nodeController,
                  label: '节点 ID',
                  onSubmitted: (_) => _refresh(resetPage: true)),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/uploads/media',
            query: cleanQuery({
              'page': page,
              'page_size': pageSize,
              'status': statusFilter,
              'keyword': keywordController.text,
              'node_id': nodeController.text,
            }),
          ),
          builder: (context, data) {
            final payload = (data as Map).cast<String, Object?>();
            final rows = rowsFrom(payload['items']);
            final total = (payload['total'] as num?)?.toInt() ?? rows.length;
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Align(
                  alignment: Alignment.centerRight,
                  child: PagerBar(
                      page: page,
                      pageSize: pageSize,
                      total: total,
                      onPageChanged: (value) {
                        page = value;
                        _refresh();
                      }),
                ),
                Surface(
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      final nameWidth =
                          constraints.maxWidth < 760 ? 180.0 : 260.0;
                      final urlWidth =
                          constraints.maxWidth < 760 ? 240.0 : 420.0;
                      return SingleChildScrollView(
                        scrollDirection: Axis.horizontal,
                        child: DataTable(
                          dataRowMinHeight: 56,
                          dataRowMaxHeight: 132,
                          columns: const [
                            DataColumn(label: Text('文件')),
                            DataColumn(label: Text('节点')),
                            DataColumn(label: Text('大小')),
                            DataColumn(label: Text('时长')),
                            DataColumn(label: Text('状态')),
                            DataColumn(label: Text('HTTP 地址')),
                            DataColumn(label: Text('操作')),
                          ],
                          rows: rows.map((row) {
                            final url = '${row['http_url'] ?? ''}';
                            return DataRow(cells: [
                              DataCell(WrappedTextCell(
                                  value: row['file_name'],
                                  maxWidth: nameWidth)),
                              DataCell(WrappedTextCell(
                                  value: row['node_name'] ?? row['node_id'],
                                  maxWidth: 180)),
                              DataCell(Text(bytesLabel(row['file_size']))),
                              DataCell(Text('${row['duration_sec'] ?? 0}s')),
                              DataCell(StatusBadge(status: row['status'])),
                              DataCell(WrappedTextCell(
                                  value: row['http_url'],
                                  maxWidth: urlWidth,
                                  selectable: true)),
                              DataCell(_UploadActions(
                                  row: row, url: url, onDone: _refresh)),
                            ]);
                          }).toList(),
                        ),
                      );
                    },
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }

  Future<void> _upload(AppController controller) async {
    setState(() {
      uploading = true;
      result = null;
    });
    try {
      final value = await controller.uploadMedia(fileController.text);
      setState(() {
        result = prettyJson(value);
        refreshSeed++;
      });
    } catch (cause) {
      setState(() => result = cause.toString());
    } finally {
      if (mounted) setState(() => uploading = false);
    }
  }
}

class _UploadActions extends StatelessWidget {
  const _UploadActions(
      {required this.row, required this.url, required this.onDone});

  final Map<String, Object?> row;
  final String url;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final hasUrl = url.startsWith('http://') || url.startsWith('https://');
    final deleted = row['status'] == 'deleted' || row['file_deleted'] == true;
    return Wrap(
      spacing: 4,
      runSpacing: 4,
      children: [
        TextButton.icon(
          onPressed: hasUrl ? () => _open(context) : null,
          icon: const Icon(Icons.play_arrow),
          label: const Text('打开'),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: hasUrl ? () => copyText(context, url) : null,
          icon: const Icon(Icons.copy),
        ),
        TextButton(
          onPressed: deleted ? null : () => _delete(context, deleteFile: false),
          child: const Text('删台账'),
        ),
        TextButton(
          onPressed: deleted ? null : () => _delete(context, deleteFile: true),
          child: const Text('删文件'),
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: textValue(row['file_name']));
    showResult(context, '已打开内嵌播放器');
  }

  Future<void> _delete(BuildContext context, {required bool deleteFile}) async {
    final name = textValue(row['file_name']);
    final confirmed = await confirmAction(
      context,
      title: deleteFile ? '删除上传文件' : '删除上传台账',
      message: deleteFile
          ? '确认删除 $name 的台账并同步删除底层文件？这可能影响历史任务和外部引用。'
          : '确认仅删除 $name 的上传台账？底层文件会保留。',
      confirmLabel: deleteFile ? '删文件' : '删台账',
      destructive: true,
    );
    if (!confirmed) return;
    if (!context.mounted) return;
    final controller = AppScope.of(context);
    try {
      await controller.api(
        'DELETE',
        '/api/v1/uploads/media/${row['id']}',
        query: {'delete_file': deleteFile},
      );
      if (context.mounted) showResult(context, '删除请求已提交');
      onDone();
    } catch (cause) {
      if (context.mounted) showResult(context, cause.toString());
    }
  }
}
