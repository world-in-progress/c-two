import json
import src.c_two as cc
import pyarrow as pa
from typing import Any, List

# Define transferables ##################################################

@cc.transferable
class UploadInfo:
    def serialize(file_path: str, file_type: str) -> bytes:
        schema = pa.schema([
            pa.field('file_path', pa.string()),
            pa.field('file_type', pa.string())
        ])
        
        data = {
            'file_path': file_path,
            'file_type': file_type
        }
        
        table = pa.Table.from_pylist([data], schema=schema)
        return cc.message.serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> tuple[str, str]:
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return (
            row['file_path'],
            row['file_type']
        )

# @cc.transferable
# class UploadResult:    
#     def serialize(info: dict[str, bool | str]) -> bytes:
#         import logging
#         logger = logging.getLogger(__name__)
#         logger.info(f'Saving grid info: {info}', len(json.dumps(info).encode('utf-8')))
#         return json.dumps(info).encode('utf-8')

#     def deserialize(res_bytes: memoryview) -> dict[str, bool | str]:
#         res = json.loads(res_bytes.tobytes().decode('utf-8'))
#         return res

@cc.transferable
class SaveInfo:
    def serialize(file_path: str, feature_json: str, is_edited: bool) -> bytes:
        schema = pa.schema([
            pa.field('file_path', pa.string()),
            pa.field('feature_json', pa.string()),
            pa.field('is_edited', pa.bool_())
        ])
        
        data = {
            'file_path': file_path,
            'feature_json': feature_json,
            'is_edited': is_edited
        }
        
        table = pa.Table.from_pylist([data], schema=schema)
        return cc.message.serialize_from_table(table)

    def deserialize(arrow_bytes: bytes) -> tuple[str, str, bool]:
        row = cc.message.deserialize_to_rows(arrow_bytes)[0]
        return (
            row['file_path'],
            row['feature_json'],
            row['is_edited']
        )

@cc.transferable
class SaveResult:
    def serialize(info: dict[str, bool | str]) -> bytes:
        return json.dumps(info).encode('utf-8')

    def deserialize(res_bytes: memoryview) -> dict[str, bool | str]:
        
        # logger = cc.logging.getLogger(__name__)
        # logger.config(level=cc.logging.DEBUG)
        # logger.debug(f'{res_bytes}, {res_bytes.tobytes()}, {len(res_bytes.tobytes())}')
        res = json.loads(res_bytes.tobytes().decode('utf-8'))
        # return {
        #     'success': res['success'],
        #     'message': res['message'],
        #     'resource_path': res['resource_path']
        # }
        return res

# Define ICRM ###########################################################
@cc.icrm
class IFeature:
    """
    ICRM
    =
    Interface of Core Resource Model (ICRM) specifies how to interact with CRM. 
    """
    def upload_feature(self, file_path: str, file_type: str) -> dict[str, bool | str]:
        ...

    def save_vector_feature(self, file_path: str, feature_json: str, is_edited: bool) -> dict[str, bool | str]:
        ...
