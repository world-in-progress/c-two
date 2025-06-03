import src.c_two as cc
from ifeature import IFeature, UploadInfo, SaveInfo
import logging
import os
import json
import shutil
from pathlib import Path
from typing import Any

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

@cc.iicrm
class Feature(IFeature):
    """
    CRM
    =
    The Feature Resource.  
    Feature is a feature file that can be uploaded to the resource pool.  
    """
    def __init__(self, feature_path: str):
        """Method to initialize Feature

        Args:
            feature_path (str): path to the feature file
        """
        self.feature_path = Path(feature_path)
        os.makedirs(feature_path, exist_ok=True)
        logger.info(f'Feature initialized with feature path: {feature_path}')

    def upload_feature(self, file_path: str, file_type: str) -> dict[str, bool | str]:
        """
        Upload a feature file to the resource pool
        """

        logger.info(f'Uploading feature in crm: {file_path}-{file_type}')

        if file_type == 'json':
            return {
                'success': True,
                'file_path': file_path,
            }
        elif file_type == 'shp':
            # TODO: 将shp文件转换为geojson
            logger.info(f'Converting shp file to geojson: {file_path}')
            return {
                'success': True,
                'file_path': str(self.feature_path / file_path),
            }
        else:
            return {
                'success': False,
                'file_path': '',
            }

    def save_vector_feature(self, file_path: str, feature_json: str, is_edited: bool) -> dict[str, bool | str]:
        """
        Save feature to resource pool
        """
        try:
            # 如果文件被编辑，复制到资源池
            if is_edited:
                # 获取文件名
                file_name = os.path.basename(file_path)
                # 复制文件到资源池
                target_path = os.path.join(self.feature_path, file_name)
                # 将json写入
                with open(target_path, 'w') as f:
                    f.write(feature_json)
                resource_path = target_path
            else:
                # 如果文件未被编辑，直接使用原路径
                resource_path = file_path
            
            # 调用资源树挂载接口
            # TODO: 实现资源树挂载逻辑
            
            return {
                'success': True,
                'message': "Feature saved successfully",
                'resource_path': resource_path
            }
        except Exception as e:
            logger.error(f'Failed to save feature: {str(e)}')
            return {
                'success': False,
                'message': str(e),
                'resource_path': ""
            }